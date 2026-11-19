//! Preserve complete MP4 files; repair the legacy ftyp + moov + unboxed payload layout.
use anyhow::{Context, Result};
use std::{
    convert::TryInto,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

fn header(file: &mut File, offset: u64, end: u64) -> Result<([u8; 4], u64, u64)> {
    anyhow::ensure!(
        offset.checked_add(8).map_or(false, |next| next <= end),
        "Truncated MP4 box"
    );
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = [0; 8];
    file.read_exact(&mut bytes)?;
    let tag = bytes[4..].try_into().unwrap();
    let size = u32::from_be_bytes(bytes[..4].try_into().unwrap());
    let (size, header_len) = match size {
        0 => (end - offset, 8),
        1 => {
            let mut bytes = [0; 8];
            file.read_exact(&mut bytes)?;
            (u64::from_be_bytes(bytes), 16)
        }
        size => (size as u64, 8),
    };
    anyhow::ensure!(
        size >= header_len && offset.checked_add(size).map_or(false, |next| next <= end),
        "Invalid MP4 box size"
    );
    Ok((tag, size, header_len))
}

pub(super) fn write_container(source: &Path, output: &Path) -> Result<()> {
    let mut file = File::open(source)?;
    let length = file.metadata()?.len();
    let mut pos = 0;
    let mut moov = false;
    let mut mdat = false;
    let complete = loop {
        if pos == length {
            break moov && mdat;
        }
        match header(&mut file, pos, length) {
            Ok((tag, size, _)) => {
                moov |= tag == *b"moov";
                mdat |= tag == *b"mdat";
                pos += size;
            }
            Err(_) => break false,
        }
    };
    if complete {
        std::fs::copy(source, output)?;
        return Ok(());
    }
    // E1 compatibility: no mdat header, only a bounded metadata prefix is held in RAM.
    let (tag, ftyp_size, _) = header(&mut file, 0, length)?;
    anyhow::ensure!(
        tag == *b"ftyp" && ftyp_size <= 1024,
        "Unsupported incomplete MP4 layout"
    );
    let (tag, moov_size, _) = header(&mut file, ftyp_size, length)?;
    anyhow::ensure!(
        tag == *b"moov" && moov_size <= 64 * 1024 * 1024,
        "Expected a bounded moov before raw payload"
    );
    let payload_offset = ftyp_size + moov_size;
    // A truncated existing mdat must not be wrapped again as if it were unboxed media.
    file.seek(SeekFrom::Start(payload_offset))?;
    let mut probe = [0; 8];
    if file.read(&mut probe)? == 8 {
        anyhow::ensure!(
            &probe[4..] != b"mdat",
            "Truncated mdat; retaining raw partial container"
        );
    }
    let payload_size = length - payload_offset;
    let header_len = if payload_size <= u32::MAX as u64 - 8 {
        8
    } else {
        16
    };
    file.seek(SeekFrom::Start(ftyp_size))?;
    let mut metadata = vec![0; moov_size as usize];
    file.read_exact(&mut metadata)?;
    relocate_offsets(&mut metadata, payload_offset + header_len)?;
    let mut destination = File::create(output)?;
    file.seek(SeekFrom::Start(0))?;
    std::io::copy(&mut (&mut file).take(ftyp_size), &mut destination)?;
    destination.write_all(&metadata)?;
    if header_len == 8 {
        destination.write_all(&((payload_size + 8) as u32).to_be_bytes())?;
        destination.write_all(b"mdat")?;
    } else {
        destination.write_all(&1u32.to_be_bytes())?;
        destination.write_all(b"mdat")?;
        destination.write_all(&(payload_size + 16).to_be_bytes())?;
    }
    file.seek(SeekFrom::Start(payload_offset))?;
    std::io::copy(&mut file, &mut destination).context("Copy MP4 media payload")?;
    destination.sync_all()?;
    Ok(())
}

fn relocate_offsets(moov: &mut [u8], new_payload: u64) -> Result<()> {
    let mut entries = Vec::new();
    for i in 4..moov.len().saturating_sub(12) {
        let width = if &moov[i..i + 4] == b"stco" {
            4
        } else if &moov[i..i + 4] == b"co64" {
            8
        } else {
            continue;
        };
        let box_start = i - 4;
        let size = u32::from_be_bytes(moov[box_start..i].try_into().unwrap()) as usize;
        let (size, count_offset, first_entry, minimum) = if size == 1 {
            if i + 20 > moov.len() {
                continue;
            }
            let size = u64::from_be_bytes(moov[i + 4..i + 12].try_into().unwrap());
            if size > usize::MAX as u64 {
                continue;
            }
            (size as usize, i + 16, i + 20, 24)
        } else {
            (size, i + 8, i + 12, 16)
        };
        if size < minimum
            || box_start
                .checked_add(size)
                .map_or(true, |end| end > moov.len())
        {
            continue;
        }
        let count =
            u32::from_be_bytes(moov[count_offset..count_offset + 4].try_into().unwrap()) as usize;
        if count > (size - minimum) / width {
            continue;
        }
        for entry in 0..count {
            entries.push((first_entry + entry * width, width));
        }
    }
    let value = |data: &[u8], offset: usize, width: usize| -> u64 {
        if width == 4 {
            u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as u64
        } else {
            u64::from_be_bytes(data[offset..offset + 8].try_into().unwrap())
        }
    };
    let Some(old_payload) = entries
        .iter()
        .map(|(offset, width)| value(moov, *offset, *width))
        .min()
    else {
        log::warn!("Replay: moov has no chunk offsets; legacy MP4 may require further camera-specific metadata");
        return Ok(());
    };
    // Use the same delta for all tracks; independently rebasing audio would destroy interleaving.
    let delta = new_payload as i128 - old_payload as i128;
    for (offset, width) in entries {
        let new = value(moov, offset, width) as i128 + delta;
        anyhow::ensure!(
            new >= 0 && new <= u64::MAX as i128 && (width == 8 || new <= u32::MAX as i128),
            "MP4 chunk offset overflow"
        );
        if width == 4 {
            moov[offset..offset + 4].copy_from_slice(&(new as u32).to_be_bytes());
        } else {
            moov[offset..offset + 8].copy_from_slice(&(new as u64).to_be_bytes());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn boxed(tag: &[u8; 4], data: &[u8]) -> Vec<u8> {
        [&((data.len() + 8) as u32).to_be_bytes()[..], tag, data].concat()
    }
    #[test]
    fn preserves_complete_mp4_and_repairs_legacy_raw_media() {
        let directory =
            std::env::temp_dir().join(format!("neolink-container-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let source = directory.join("source");
        let output = directory.join("output");
        let ftyp = boxed(b"ftyp", b"isom0000");
        let moov = boxed(b"moov", &[]);
        let complete = [ftyp.clone(), boxed(b"mdat", b"media"), moov.clone()].concat();
        std::fs::write(&source, &complete).unwrap();
        write_container(&source, &output).unwrap();
        assert_eq!(std::fs::read(&output).unwrap(), complete);
        let raw = [ftyp.clone(), moov.clone(), b"unboxed video".to_vec()].concat();
        std::fs::write(&source, raw).unwrap();
        write_container(&source, &output).unwrap();
        assert_eq!(
            std::fs::read(&output).unwrap(),
            [ftyp, moov, boxed(b"mdat", b"unboxed video")].concat()
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn relocation_keeps_audio_video_offsets_relative() {
        let mut data = Vec::new();
        for offset in [100u32, 120u32] {
            data.extend(boxed(
                b"stco",
                &[&[0u8; 4][..], &1u32.to_be_bytes(), &offset.to_be_bytes()].concat(),
            ));
        }
        relocate_offsets(&mut data, 108).unwrap();
        assert_eq!(u32::from_be_bytes(data[16..20].try_into().unwrap()), 108);
        assert_eq!(u32::from_be_bytes(data[36..40].try_into().unwrap()), 128);
    }
}
