//! Replay format selection. BC message boundaries are not BcMedia frame boundaries.
use super::{strip_e1_replay_envelope, Result};
use crate::bcmedia::{codex::BcMediaCodex, model::*};
use crate::Error;
use bytes::{Buf, BytesMut};
use std::convert::TryInto;
use tokio_util::codec::Decoder;

pub(super) struct ReplayDecoder {
    codec: BcMediaCodex,
    pub(super) buffer: BytesMut,
    format: Option<ReplayFormat>,
    legacy_header: bool,
    expected_size: Option<u64>,
    raw_bytes: u64,
}

fn bcmedia_start(data: &[u8]) -> bool {
    data.get(..4).map_or(false, |bytes| {
        matches!(u32::from_le_bytes(bytes.try_into().unwrap()),
            MAGIC_HEADER_BCMEDIA_INFO_V1 | MAGIC_HEADER_BCMEDIA_INFO_V2 |
            MAGIC_HEADER_BCMEDIA_IFRAME..=MAGIC_HEADER_BCMEDIA_IFRAME_LAST |
            MAGIC_HEADER_BCMEDIA_PFRAME..=MAGIC_HEADER_BCMEDIA_PFRAME_LAST |
            MAGIC_HEADER_BCMEDIA_AAC | MAGIC_HEADER_BCMEDIA_ADPCM)
    })
}

fn detect(data: &[u8]) -> Option<ReplayFormat> {
    if bcmedia_start(data) {
        return Some(ReplayFormat::BcMedia);
    }
    if data.len() >= 8 {
        let size = u32::from_be_bytes(data[..4].try_into().unwrap());
        if (size == 0 || size == 1 || size >= 8)
            && matches!(
                &data[4..8],
                b"ftyp" | b"moov" | b"mdat" | b"styp" | b"moof" | b"free" | b"wide"
            )
        {
            return Some(ReplayFormat::Mp4);
        }
    }
    if data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1]) {
        return Some(ReplayFormat::AnnexB);
    }
    // Only unframed H.264 packets use this path; never apply it to MP4 continuation bytes.
    if data.len() >= 2
        && matches!(
            data[0],
            0x01 | 0x21 | 0x41 | 0x61 | 0x65 | 0x67 | 0x68 | 0x06 | 0x09
        )
    {
        return Some(ReplayFormat::H264Nal);
    }
    None
}

impl ReplayDecoder {
    pub(super) fn new(strict: bool) -> Self {
        Self {
            codec: BcMediaCodex::new(strict),
            buffer: BytesMut::new(),
            format: None,
            legacy_header: false,
            expected_size: None,
            raw_bytes: 0,
        }
    }

    pub(super) fn with_options(
        strict: bool,
        expected_size: Option<u64>,
        legacy_header: bool,
    ) -> Self {
        Self {
            expected_size: expected_size.filter(|size| *size > 0),
            legacy_header,
            ..Self::new(strict)
        }
    }

    pub(super) fn decode(&mut self, data: &[u8]) -> Result<Vec<BcMedia>> {
        let data = strip_e1_replay_envelope(data);
        if data.is_empty() {
            return Ok(Vec::new());
        }
        if self.format.is_some() && self.format != Some(ReplayFormat::BcMedia) {
            return Ok(self.raw(data.to_vec()));
        }
        self.buffer.extend_from_slice(data);
        let mut frames = Vec::new();
        if self.format.is_none() {
            let mut selected = detect(&self.buffer);
            // A 32-byte prefix is removed only after a positive format match at +32.
            // InfoV1/V2 starts at +0 and therefore cannot be mistaken for that prefix.
            if selected.is_none() && self.buffer.len() >= 40 {
                if let Some(format) = detect(&self.buffer[32..]) {
                    if (self.legacy_header || format == ReplayFormat::Mp4)
                        && self.expected_size.is_none()
                    {
                        self.expected_size = [16, 24].iter().find_map(|off| {
                            let size =
                                u64::from_le_bytes(self.buffer[*off..*off + 8].try_into().unwrap());
                            (size > 0 && size <= 500_000_000).then_some(size)
                        });
                    }
                    self.buffer.advance(32);
                    selected = Some(format);
                }
            }
            if selected.is_none()
                && (self.buffer.len() < 8
                    || (32..40).contains(&self.buffer.len())
                    || (self.legacy_header && self.buffer.len() <= 32))
            {
                return Ok(frames);
            }
            let selected = selected.unwrap_or(ReplayFormat::Opaque);
            log::info!("Replay: selected format {:?}", selected);
            self.format = Some(selected);
            if selected != ReplayFormat::BcMedia {
                frames.push(BcMedia::ReplayFormat(selected));
                let data = self.buffer.split().to_vec();
                frames.extend(self.raw(data));
                return Ok(frames);
            }
        }
        // Bound an incomplete/corrupt frame rather than growing indefinitely.
        if self.buffer.len() > 64 * 1024 * 1024 {
            return Err(Error::Other("Replay BcMedia frame exceeds 64 MiB"));
        }
        while let Some(frame) = self.codec.decode(&mut self.buffer)? {
            frames.push(frame);
        }
        Ok(frames)
    }

    fn raw(&mut self, mut data: Vec<u8>) -> Vec<BcMedia> {
        self.raw_bytes += data.len() as u64;
        if self.format == Some(ReplayFormat::H264Nal)
            && !data.starts_with(&[0, 0, 1])
            && !data.starts_with(&[0, 0, 0, 1])
        {
            data.splice(..0, [0, 0, 0, 1]);
        }
        vec![BcMedia::RawReplayChunk(data)]
    }

    pub(super) fn observe_metadata(&mut self, msg: &crate::bc::model::Bc) {
        use crate::bc::{
            model::{BcBody, ModernMsg},
            xml::BcPayloads,
        };
        if let BcBody::ModernMsg(ModernMsg {
            payload: Some(BcPayloads::BcXml(xml)),
            ..
        }) = &msg.body
        {
            if let Some(list) = &xml.file_info_list {
                if list.file_info.len() == 1 && list.file_info[0].file_count.unwrap_or(1) <= 1 {
                    let info = &list.file_info[0];
                    let size =
                        info.size_l.unwrap_or(0) as u64 + ((info.size_h.unwrap_or(0) as u64) << 32);
                    if size > 0 {
                        self.expected_size = Some(size);
                    }
                }
            }
        }
    }

    /// Stored MP4 size is not a BcMedia transfer size.
    pub(super) fn reached_size(&self) -> bool {
        matches!(self.format, Some(ReplayFormat::Mp4 | ReplayFormat::Opaque))
            && self
                .expected_size
                .map_or(false, |size| self.raw_bytes >= size)
    }

    pub(super) fn finish(&mut self) -> Result<Vec<BcMedia>> {
        if self.format.is_none() && !self.buffer.is_empty() {
            self.format = Some(ReplayFormat::Opaque);
            let data = self.buffer.split().to_vec();
            return Ok(vec![
                BcMedia::ReplayFormat(ReplayFormat::Opaque),
                BcMedia::RawReplayChunk(data),
            ]);
        }
        if !self.buffer.is_empty() {
            return Err(Error::Other(
                "Replay ended with an incomplete BcMedia frame",
            ));
        }
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn raw(frames: Vec<BcMedia>) -> Vec<u8> {
        frames
            .into_iter()
            .flat_map(|frame| match frame {
                BcMedia::RawReplayChunk(data) => data,
                _ => Vec::new(),
            })
            .collect()
    }
    #[test]
    fn detects_mp4_across_bc_messages_and_preserves_continuation() {
        let mp4 = b"\0\0\0\x10ftypisom\0\0\0\0";
        for split in 1..8 {
            let mut decoder = ReplayDecoder::new(true);
            let mut bytes = raw(decoder.decode(&mp4[..split]).unwrap());
            bytes.extend(raw(decoder.decode(&mp4[split..]).unwrap()));
            bytes.extend(raw(decoder.decode(b"\x41continuation").unwrap()));
            assert_eq!(bytes, [mp4.as_slice(), b"\x41continuation"].concat());
        }
    }
    #[test]
    fn desktop_mp4_with_legacy_prefix_is_detected_without_losing_container_bytes() {
        let mut decoder = ReplayDecoder::with_options(true, None, false);
        assert!(decoder.decode(&[0xff; 32]).unwrap().is_empty());
        let mp4 = b"\0\0\0\x10ftypisom\0\0\0\0";
        assert!(decoder.decode(&mp4[..4]).unwrap().is_empty());
        assert_eq!(raw(decoder.decode(&mp4[4..]).unwrap()), mp4);
    }
    #[test]
    fn legacy_size_applies_to_container_only() {
        let mut header = [0xff; 32];
        header[16..24].copy_from_slice(&16u64.to_le_bytes());
        let mut decoder = ReplayDecoder::with_options(true, None, true);
        assert!(decoder.decode(&header).unwrap().is_empty());
        let bytes = b"\0\0\0\x10ftypisom\0\0\0\0";
        assert_eq!(raw(decoder.decode(bytes).unwrap()), bytes);
        assert!(decoder.reached_size());
        let mut decoder = ReplayDecoder::with_options(true, Some(1), false);
        decoder.decode(b"00dcH264").unwrap();
        assert!(!decoder.reached_size());
    }
    #[test]
    fn raw_nals_and_annex_b_are_kept() {
        let nal = b"\x65raw H264 NAL";
        let mut decoder = ReplayDecoder::new(true);
        assert_eq!(
            raw(decoder.decode(nal).unwrap()),
            [b"\0\0\0\x01".as_slice(), nal].concat()
        );
        assert_eq!(
            raw(decoder.decode(b"\0\0\x01\x41frame").unwrap()),
            b"\0\0\x01\x41frame"
        );
        let mut decoder = ReplayDecoder::new(true);
        assert_eq!(
            raw(decoder.decode(b"\0\0\0\x01\x65frame").unwrap()),
            b"\0\0\0\x01\x65frame"
        );
    }
    #[test]
    fn unknown_payload_is_preserved_and_short_payload_flushed() {
        let mut decoder = ReplayDecoder::new(false);
        assert_eq!(
            raw(decoder.decode(b"\xfeopaque payload").unwrap()),
            b"\xfeopaque payload"
        );
        let mut decoder = ReplayDecoder::new(false);
        assert!(decoder.decode(b"xyz").unwrap().is_empty());
        assert_eq!(raw(decoder.finish().unwrap()), b"xyz");
    }
}
