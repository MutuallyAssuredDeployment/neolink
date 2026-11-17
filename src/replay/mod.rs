//!
//! # Neolink Replay
//!
//! List recording days and files from SD card, start/stop playback (MSG 142, 14, 15, 5, 7).
//!
//! # Usage
//!
//! ```bash
//! neolink replay days --config=config.toml CameraName --start 2024-02-01 [--end 2024-02-07]
//! neolink replay files --config=config.toml CameraName --date 2024-02-04 [--stream subStream]
//! neolink replay play --config=config.toml CameraName --name "01_20240204120000" [--output out.h264]
//! neolink replay stop --config=config.toml CameraName --name "01_20240204120000"
//! ```
//!

use anyhow::{Context, Result};
use std::convert::TryInto;
use std::path::Path;

mod cmdline;
mod container;
mod output;

#[cfg(feature = "gstreamer")]
mod gst;

/// Annex B start code (4-byte) for H.264/HEVC NAL.
const ANNEX_B_START: &[u8] = &[0x00, 0x00, 0x00, 0x01];

/// Scan for the first BcMedia 4-byte LE magic (InfoV1/2, IFRAME, PFRAME, AAC). Returns offset or None.
#[cfg(test)]
fn find_bcmedia_magic_offset(stream: &[u8]) -> Option<usize> {
    const MAGIC_INFO_V1: u32 = 0x31303031;
    const MAGIC_INFO_V2: u32 = 0x32303031;
    const MAGIC_IFRAME: u32 = 0x63643030;
    const MAGIC_IFRAME_LAST: u32 = 0x63643039;
    const MAGIC_PFRAME: u32 = 0x63643130;
    const MAGIC_PFRAME_LAST: u32 = 0x63643139;
    const MAGIC_AAC: u32 = 0x62773530;
    for i in 0..stream.len().saturating_sub(4) {
        let magic = u32::from_le_bytes(stream[i..i + 4].try_into().unwrap());
        if magic == MAGIC_INFO_V1
            || magic == MAGIC_INFO_V2
            || (MAGIC_IFRAME..=MAGIC_IFRAME_LAST).contains(&magic)
            || (MAGIC_PFRAME..=MAGIC_PFRAME_LAST).contains(&magic)
            || magic == MAGIC_AAC
        {
            return Some(i);
        }
    }
    None
}

/// True if payload looks like a valid H.264 or H.265 NAL. Drops garbage from resync false positives.
#[cfg(test)]
fn is_likely_valid_nal(payload: &[u8], video_type: VideoType) -> bool {
    if payload.is_empty() {
        return false;
    }
    let nal_header = if payload.len() >= 5 && payload[0..4] == [0x00, 0x00, 0x00, 0x01] {
        &payload[4..]
    } else if payload.len() >= 4 && payload[0..3] == [0x00, 0x00, 0x01] {
        &payload[3..]
    } else {
        payload
    };
    match video_type {
        VideoType::H264 => (1..=9).contains(&(nal_header[0] & 0x1F)),
        VideoType::H265 => {
            nal_header.len() >= 2 && nal_header[0] & 0x80 == 0 && (nal_header[0] >> 1) <= 40
        }
    }
}

/// Max bytes to advance on Incomplete without finding any NAL. Prevents hanging on garbage/ciphertext (e.g. undecrypted replay).
#[cfg(test)]
const BCMEDIA_RESYNC_CAP: usize = 128 * 1024;

/// Decode a buffer (e.g. after stream_for_mp4_assembly) as BcMedia and collect video NAL payloads.
/// Returns Some((nals, fps)) if at least one IFRAME/PFRAME was decoded; None if stream is not BcMedia or has no video.
/// fps is extracted from the BcMediaInfoV1/V2 header if present, otherwise defaults to 25.
/// When the codec returns Incomplete (Ok(None)) we resync to the next frame and continue, so a full buffer is decoded.
/// Only payloads that look like valid H.264/H.265 NALs are collected to avoid garbage from resync.
/// See notes/REPLAY_VIDEO_FORMAT.md.
/// Result of decoding a BcMedia stream.
#[cfg(test)]
struct BcMediaDecoded {
    nals: Vec<Vec<u8>>,
    video_type: VideoType,
    fps: u8,
    /// Per-frame microsecond timestamps from BcMedia headers (same length as `nals`).
    timestamps_us: Vec<u32>,
    /// Concatenated AAC ADTS frames (empty if no audio in stream).
    aac_data: Vec<u8>,
}

#[cfg(test)]
fn try_decode_bcmedia_nals(stream: &[u8]) -> Option<BcMediaDecoded> {
    let mut codec = BcMediaCodex::new(false);
    let mut buf = BytesMut::from(stream);
    let mut nals: Vec<Vec<u8>> = Vec::new();
    let mut timestamps_us: Vec<u32> = Vec::new();
    let mut fps: u8 = 25; // default; overridden by InfoV1/V2 if present
    let mut video_type = VideoType::H264;
    let mut aac_data: Vec<u8> = Vec::new();
    let start_len = buf.len();
    let mut bytes_advanced_without_nal: usize = 0;
    loop {
        match codec.decode(&mut buf) {
            Ok(Some(BcMedia::Iframe(BcMediaIframe {
                data,
                microseconds,
                video_type: frame_type,
                ..
            }))) => {
                if is_likely_valid_nal(&data, frame_type) {
                    nals.push(data);
                    timestamps_us.push(microseconds);
                    video_type = frame_type;
                    bytes_advanced_without_nal = 0;
                }
            }
            Ok(Some(BcMedia::Pframe(BcMediaPframe {
                data,
                microseconds,
                video_type: frame_type,
                ..
            }))) => {
                if is_likely_valid_nal(&data, frame_type) {
                    nals.push(data);
                    timestamps_us.push(microseconds);
                    video_type = frame_type;
                    bytes_advanced_without_nal = 0;
                }
            }
            Ok(Some(BcMedia::InfoV1(BcMediaInfoV1 { fps: f, .. })))
            | Ok(Some(BcMedia::InfoV2(BcMediaInfoV2 { fps: f, .. }))) => {
                if f > 0 {
                    log::info!("Replay: BcMedia stream fps={} (from InfoV1/V2 header)", f);
                    fps = f;
                }
            }
            Ok(Some(BcMedia::Aac(BcMediaAac { data, .. }))) => {
                aac_data.extend_from_slice(&data);
            }
            Ok(Some(_)) => {}
            Err(_) => break,
            Ok(None) => {
                if buf.remaining() < 4 {
                    break;
                }
                buf.advance(1);
                bytes_advanced_without_nal += 1;
                if nals.is_empty() && bytes_advanced_without_nal >= BCMEDIA_RESYNC_CAP {
                    log::debug!(
                        "Replay: BcMedia decode advanced {} bytes without finding a frame, stopping (likely garbage/undecrypted)",
                        start_len - buf.len()
                    );
                    break;
                }
            }
        }
    }
    if !aac_data.is_empty() {
        log::info!(
            "Replay: collected {} bytes of AAC audio from BcMedia stream",
            aac_data.len()
        );
    }
    if nals.is_empty() {
        None
    } else {
        Some(BcMediaDecoded {
            nals,
            video_type,
            fps,
            timestamps_us,
            aac_data,
        })
    }
}

/// Compute actual average fps from per-frame microsecond timestamps. Returns None if < 2 frames.
#[cfg(test)]
fn compute_actual_fps(timestamps_us: &[u32]) -> Option<f64> {
    if timestamps_us.len() < 2 {
        return None;
    }
    // Handle u32 wraparound: timestamps are relative to stream start and may wrap at ~4295s (~71 min).
    // Use deltas between consecutive frames to avoid wraparound issues.
    let mut total_delta_us: u64 = 0;
    let mut valid_deltas: u64 = 0;
    for pair in timestamps_us.windows(2) {
        let delta = pair[1].wrapping_sub(pair[0]);
        // Reject implausible deltas (> 2 seconds or 0) as likely wraparound artifacts or duplicates.
        if delta > 0 && delta < 2_000_000 {
            total_delta_us += delta as u64;
            valid_deltas += 1;
        }
    }
    if valid_deltas == 0 || total_delta_us == 0 {
        return None;
    }
    let avg_frame_duration_us = total_delta_us as f64 / valid_deltas as f64;
    let fps = 1_000_000.0 / avg_frame_duration_us;
    // Sanity: fps should be in 1..120 range
    if fps >= 1.0 && fps <= 120.0 {
        Some(fps)
    } else {
        None
    }
}

/// Recording metadata for MP4 embedding.
#[derive(Debug, Default, Clone)]
struct RecordingMeta {
    /// Camera recording type / AI detection tags (e.g. "manual,sched,md,people,vehicle").
    pub record_type: Option<String>,
    /// Recording start time as "YYYY-MM-DD HH:MM:SS".
    pub start_time: Option<String>,
    /// Recording end time as "YYYY-MM-DD HH:MM:SS".
    pub end_time: Option<String>,
    /// Camera name / channel.
    pub camera_name: Option<String>,
}

/// Format a ReplayDateTime as "YYYY-MM-DD HH:MM:SS".
fn format_replay_datetime(dt: &ReplayDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second
    )
}

/// Write H.264/H.265 NAL payloads as Annex B. Existing start codes are preserved.
#[cfg(test)]
fn annex_b_from_nals(nals: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nals {
        let has_start = (nal.len() >= 3 && nal[0..3] == [0x00, 0x00, 0x01])
            || (nal.len() >= 4 && nal[0..4] == [0x00, 0x00, 0x00, 0x01]);
        if !has_start {
            out.extend_from_slice(ANNEX_B_START);
        }
        out.extend_from_slice(nal);
    }
    out
}

/// Annex B start codes: 0x00 0x00 0x01 or 0x00 0x00 0x00 0x01.
/// Kept for potential use when scanning raw buffers for Annex B.
#[allow(dead_code)]
fn find_annexb_h264_start(buffer: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i + 3 <= buffer.len() {
        if buffer[i..i + 3] == [0x00, 0x00, 0x01] {
            return Some(i);
        }
        if i + 4 <= buffer.len() && buffer[i..i + 4] == [0x00, 0x00, 0x00, 0x01] {
            return Some(i);
        }
        i += 1;
    }
    None
}

use crate::common::NeoReactor;
#[cfg(test)]
use bytes::{Buf, BytesMut};
use neolink_core::bc::xml::{DayRecords, FileInfo, ReplayDateTime};
#[cfg(test)]
use neolink_core::bcmedia::codex::BcMediaCodex;
use neolink_core::bcmedia::model::{BcMedia, VideoType};
use tokio::time::{sleep_until, Duration, Instant};
#[cfg(test)]
use tokio_util::codec::Decoder;

#[cfg(test)]
use neolink_core::bcmedia::model::{
    BcMediaAac, BcMediaIframe, BcMediaInfoV1, BcMediaInfoV2, BcMediaPframe,
};

pub(crate) use cmdline::Opt;

/// E1 cameras sometimes write wrong values in the avcC box (e.g. config version 0, wrong lengthSizeMinusOne),
/// which breaks decoders. Patch the first bytes so the file can play when the rest (SPS/PPS) is present.
/// Provenance: empirical (no RE). App replay path does not handle raw MP4; download path builds MP4 from
/// DATA_FRAME_DESC, so we have no Ghidra trace of the app patching avcC. See notes § Replay: full end-to-end flow and provenance.
fn patch_e1_avcc_if_needed(path: &Path) -> Result<()> {
    const AVCC_TAG: &[u8; 4] = b"avcC";
    // avcC content: [0]=configurationVersion(1), [1-3]=profile/compat/level, [4]=lengthSizeMinusOne|reserved, [5]=numSPS|reserved
    const CONTENT_CONFIG_VERSION: usize = 0;
    const CONTENT_LENGTH_SIZE_BYTE: usize = 4;
    const CONFIG_VERSION_CORRECT: u8 = 0x01;
    const LENGTH_SIZE_BYTE_CORRECT: u8 = 0x43; // lengthSizeMinusOne=3 (4-byte NAL length)

    let mut buf = [0u8; 4096];
    let mut f = std::fs::File::open(path).context("Open MP4 for avcC patch")?;
    let n = std::io::Read::read(&mut f, &mut buf).context("Read MP4 head")?;
    let head = &buf[..n];
    if let Some(pos) = head.windows(4).position(|w| w == AVCC_TAG) {
        let content_start = pos + 4; // first byte of avcC content (after "avcC" tag)
        let mut patches: Vec<(usize, u8, u8)> = Vec::new();
        if content_start + CONTENT_LENGTH_SIZE_BYTE < head.len() {
            let b4 = head[content_start + CONTENT_LENGTH_SIZE_BYTE];
            if b4 != LENGTH_SIZE_BYTE_CORRECT {
                patches.push((
                    content_start + CONTENT_LENGTH_SIZE_BYTE,
                    b4,
                    LENGTH_SIZE_BYTE_CORRECT,
                ));
            }
        }
        if content_start + CONTENT_CONFIG_VERSION < head.len() {
            let b0 = head[content_start + CONTENT_CONFIG_VERSION];
            if b0 != CONFIG_VERSION_CORRECT {
                patches.push((
                    content_start + CONTENT_CONFIG_VERSION,
                    b0,
                    CONFIG_VERSION_CORRECT,
                ));
            }
        }
        if !patches.is_empty() {
            drop(f);
            use std::io::{Seek, SeekFrom, Write};
            let mut f = std::fs::OpenOptions::new().write(true).open(path)?;
            for (fix_pos, _was, correct) in &patches {
                f.seek(SeekFrom::Start(*fix_pos as u64))?;
                f.write_all(&[*correct])?;
            }
            f.sync_all().context("Sync patched MP4 to disk")?;
            for (_, w, c) in &patches {
                log::info!(
                    "Replay: patched E1 avcC (0x{:02x} -> 0x{:02x}) for playback",
                    w,
                    c
                );
            }
        }
    }
    Ok(())
}

/// Some cameras omit colour_primaries/transfer_characteristics/matrix_coefficients in the
/// avc1 sample entry, so ffmpeg reports "unspecified pixel format" and may not start decoding.
/// Write BT.709 (1,1,1) into the optional fields so players can infer yuv420p.
/// Only patch when avc1 box is >= 92 bytes so we don't overwrite the following avcC box.
/// Provenance: empirical (no RE). See notes § Replay: full end-to-end flow and provenance.
fn patch_avc1_colour_if_needed(path: &Path) -> Result<()> {
    const AVC1_TAG: &[u8; 4] = b"avc1";
    // Optional colour at 80..86; require box >= 92 so we don't overwrite avcC.
    const COLOUR_OFFSET_IN_AVC1_BOX: usize = 80;
    const MIN_AVC1_BOX_LEN: usize = 92;
    const COLOUR_BYTES: &[u8; 6] = &[0x00, 0x01, 0x00, 0x01, 0x00, 0x01]; // BT.709

    let mut buf = [0u8; 65536];
    let mut f = std::fs::File::open(path).context("Open MP4 for avc1 colour patch")?;
    let n = std::io::Read::read(&mut f, &mut buf).context("Read MP4 for avc1")?;
    let head = &buf[..n];

    let mut box_start = 0usize;
    while let Some(tag_pos) = head[box_start..].windows(4).position(|w| w == AVC1_TAG) {
        let i = box_start + tag_pos;
        if i < 4 {
            box_start = i + 1;
            continue;
        }
        let start = i - 4;
        let size_u32 = u32::from_be_bytes(head[start..start + 4].try_into().unwrap());
        let box_len = if size_u32 == 1 {
            if start + 16 > head.len() {
                box_start = i + 1;
                continue;
            }
            u64::from_be_bytes(head[start + 8..start + 16].try_into().unwrap()) as usize
        } else {
            size_u32 as usize
        };
        if box_len >= MIN_AVC1_BOX_LEN
            && start + COLOUR_OFFSET_IN_AVC1_BOX + COLOUR_BYTES.len() <= head.len()
        {
            let colour_pos = start + COLOUR_OFFSET_IN_AVC1_BOX;
            if head[colour_pos..colour_pos + COLOUR_BYTES.len()] != COLOUR_BYTES[..] {
                drop(f);
                let mut f = std::fs::OpenOptions::new().write(true).open(path)?;
                std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(colour_pos as u64))?;
                std::io::Write::write_all(&mut f, COLOUR_BYTES)?;
                f.sync_all().context("Sync avc1 colour patch")?;
                log::info!("Replay: patched avc1 colour description (BT.709) for playback");
                return Ok(());
            }
        }
        box_start = i + 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_keeps_metadata_timer_but_downloads_are_not_limited_by_media_duration() {
        let start = ReplayDateTime {
            year: 2026,
            month: 11,
            day: 12,
            hour: 15,
            minute: 4,
            second: 47,
        };
        let metadata = FileInfo {
            start_time: Some(start.clone()),
            end_time: Some(ReplayDateTime {
                second: 54,
                ..start
            }),
            ..Default::default()
        };
        assert_eq!(replay_duration(None, false, Some(&metadata)), Some(7));
        assert_eq!(replay_duration(None, true, Some(&metadata)), None);
        assert_eq!(replay_duration(Some(5), true, Some(&metadata)), Some(5));
        assert_eq!(replay_duration(None, false, None), None);
    }

    #[test]
    fn download_retry_preserves_receive_fallback_and_does_not_retry_output_or_auth_errors() {
        assert!(retry_download_error(
            &neolink_core::Error::TimeoutDisconnected.into()
        ));
        assert!(retry_download_error(
            &neolink_core::Error::CameraServiceUnavailable { id: 143, code: 405 }.into()
        ));
        assert!(!retry_download_error(
            &neolink_core::Error::CameraServiceUnavailable { id: 143, code: 401 }.into()
        ));
        assert!(!retry_download_error(
            &std::io::Error::new(std::io::ErrorKind::PermissionDenied, "output is read-only")
                .into()
        ));
    }

    /// Minimal BcMedia IFRAME: magic 0x63643030, "H264", payload_size=5, additional_header_size=0,
    /// then 5-byte NAL (start code + SPS type) + 3-byte pad to 8-byte boundary.
    fn minimal_bcmedia_iframe() -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&0x63643030u32.to_le_bytes()); // MAGIC IFRAME
        buf.extend_from_slice(b"H264");
        buf.extend_from_slice(&5u32.to_le_bytes()); // payload_size
        buf.extend_from_slice(&0u32.to_le_bytes()); // additional_header_size
        buf.extend_from_slice(&1000u32.to_le_bytes()); // microseconds = 1ms
        buf.extend_from_slice(&0u32.to_le_bytes()); // unknown_b
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x67]); // NAL: start code + SPS (type 7)
        buf.extend_from_slice(&[0u8; 3]); // padding to 8-byte boundary
        buf
    }

    #[test]
    fn test_try_decode_bcmedia_nals_decodes_iframe() {
        let stream = minimal_bcmedia_iframe();
        let result = try_decode_bcmedia_nals(&stream);
        assert!(
            result.is_some(),
            "BcMedia decode should return Some(BcMediaDecoded)"
        );
        let decoded = result.unwrap();
        assert_eq!(decoded.nals.len(), 1);
        assert!(matches!(decoded.video_type, VideoType::H264));
        assert_eq!(decoded.nals[0], &[0x00, 0x00, 0x00, 0x01, 0x67]);
        assert_eq!(decoded.timestamps_us.len(), 1);
        assert_eq!(decoded.timestamps_us[0], 1000);
        assert_eq!(
            decoded.fps, 25,
            "Default fps should be 25 when no InfoV1/V2 header"
        );
        assert!(decoded.aac_data.is_empty(), "No audio in video-only stream");
    }

    #[test]
    fn test_compute_actual_fps() {
        // 10 frames at exactly 100ms intervals = 10 fps
        let ts: Vec<u32> = (0..10).map(|i| i * 100_000).collect();
        let fps = compute_actual_fps(&ts).unwrap();
        assert!((fps - 10.0).abs() < 0.1, "Expected ~10 fps, got {}", fps);

        // 30 frames at ~33.3ms intervals = 30 fps
        let ts: Vec<u32> = (0..30).map(|i| i * 33_333).collect();
        let fps = compute_actual_fps(&ts).unwrap();
        assert!((fps - 30.0).abs() < 0.5, "Expected ~30 fps, got {}", fps);

        // < 2 frames → None
        assert!(compute_actual_fps(&[0]).is_none());
        assert!(compute_actual_fps(&[]).is_none());

        // All same timestamp → None (0 deltas rejected)
        assert!(compute_actual_fps(&[1000, 1000, 1000]).is_none());
    }

    #[test]
    fn test_stream_for_mp4_assembly_skips_32_when_no_ftyp() {
        let mut buf = vec![0u8; 64];
        buf[0..32].fill(0xab);
        buf[32..36].copy_from_slice(&0x63643030u32.to_le_bytes());
        buf[36..40].copy_from_slice(b"H264");
        let stream = stream_for_mp4_assembly(&buf, None);
        assert_eq!(stream.len(), 32);
        assert_eq!(stream[0..4], 0x63643030u32.to_le_bytes());
    }

    /// Decode a real replay dump if present. Generate with:
    ///   uv run python scripts/extract_replay_from_pcap_app.py --password PASSWORD --debug-dump /tmp/replay.bin PCAPdroid_*.pcap
    /// Then: REPLAY_DUMP=/tmp/replay.bin cargo test test_decode_real_replay_dump -- --ignored
    /// The script's dump is already reassembled with skip_first_32, so it starts with BcMedia (no leading 32-byte header).
    #[test]
    #[ignore]
    fn test_decode_real_replay_dump() {
        let path = std::env::var("REPLAY_DUMP").ok();
        let path = match path.as_deref() {
            Some(p) => std::path::Path::new(p),
            None => return,
        };
        let raw = std::fs::read(path).expect("read replay dump");
        // Script dump may have leading bytes before first BcMedia frame; find magic then decode.
        let stream = find_bcmedia_magic_offset(&raw)
            .map(|off| &raw[off..])
            .unwrap_or_else(|| stream_for_mp4_assembly(&raw, None));
        let decoded = try_decode_bcmedia_nals(stream).expect("BcMedia decode");
        assert!(
            !decoded.nals.is_empty(),
            "should get at least one video frame"
        );
        eprintln!(
            "Replay dump: {} NALs, fps={}, aac={} bytes",
            decoded.nals.len(),
            decoded.fps,
            decoded.aac_data.len()
        );
        let annex_b = annex_b_from_nals(&decoded.nals);
        assert!(annex_b.len() > 4, "Annex B output non-empty");
    }
}

/// Replay stream used for MP4 assembly. If the buffer doesn't start with ftyp, we may skip
/// the first 32-byte replay header (app does this only for MSG 5; for 0x17d it writes the 32 bytes).
/// `skip_first_32`: Some(true) = skip 32 when no ftyp (MSG 5); Some(false) = never skip (MSG 8/0x17d); None = legacy (skip when no ftyp and len>32).
#[cfg(test)]
fn stream_for_mp4_assembly(buffer: &[u8], skip_first_32: Option<bool>) -> &[u8] {
    const REPLAY_HEADER_LEN: usize = 32;
    let starts_with_ftyp = buffer.len() >= 8 && buffer[4..8] == *b"ftyp";
    let should_skip = match skip_first_32 {
        Some(true) => buffer.len() > REPLAY_HEADER_LEN && !starts_with_ftyp,
        Some(false) => false,
        None => buffer.len() > REPLAY_HEADER_LEN && !starts_with_ftyp,
    };
    if should_skip {
        &buffer[REPLAY_HEADER_LEN..]
    } else {
        buffer
    }
}

/// Parse YYYY-MM-DD into (year, month, day).
fn parse_date(s: &str) -> Result<(i32, u8, u8)> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 {
        anyhow::bail!("Date must be YYYY-MM-DD, got {:?}", s);
    }
    let year: i32 = parts[0].parse().context("Invalid year")?;
    let month: u8 = parts[1].parse().context("Invalid month")?;
    let day: u8 = parts[2].parse().context("Invalid day")?;
    if month == 0 || month > 12 || day == 0 || day > 31 {
        anyhow::bail!("Invalid month or day");
    }
    Ok((year, month, day))
}

fn date_to_replay_start(year: i32, month: u8, day: u8) -> ReplayDateTime {
    ReplayDateTime {
        year,
        month,
        day,
        hour: 0,
        minute: 0,
        second: 0,
    }
}

fn date_to_replay_end(year: i32, month: u8, day: u8) -> ReplayDateTime {
    ReplayDateTime {
        year,
        month,
        day,
        hour: 23,
        minute: 59,
        second: 59,
    }
}

/// Common receive loop for all transports. Receive errors are returned only after cleanup.
async fn receive_recording(
    stream: &mut neolink_core::bc_protocol::StreamData,
    writer: &mut output::RecordingWriter,
    duration: Option<u64>,
) -> Result<bool> {
    let deadline = duration.map(|seconds| Instant::now() + Duration::from_secs(seconds));
    loop {
        let frame = tokio::select! {
            _ = async { match deadline { Some(deadline) => sleep_until(deadline).await, None => std::future::pending().await } } => {
                log::info!("Replay: explicit duration limit reached; retaining partial recording");
                return Ok(false);
            },
            _ = tokio::signal::ctrl_c() => {
                return Ok(false);
            },
            result = stream.get_data() => result??,
        };
        if matches!(frame, BcMedia::StreamEnd) {
            return Ok(true);
        }
        writer.push(frame).context("Write recording spool")?;
    }
}

async fn stop_recording(
    camera: &crate::common::NeoInstance,
    stream: &mut neolink_core::bc_protocol::StreamData,
    replay_name: Option<&str>,
) -> Result<()> {
    // Cancel and join the producer first; its ID 144 cleanup must finish before any retry.
    let result = stream.shutdown().await.context("Stop recording receiver");
    if let Some(name) = replay_name {
        let stop = tokio::time::timeout(
            Duration::from_secs(5),
            camera.run_task(|cam| {
                let name = name.to_string();
                Box::pin(async move { cam.replay_stop(&name).await.context("Replay stop failed") })
            }),
        )
        .await;
        if !matches!(stop, Ok(Ok(()))) {
            log::warn!("Replay: MSG 7 stop failed or timed out: {:?}", stop);
        }
    }
    result
}

async fn save_recording(
    writer: &mut output::RecordingWriter,
    output: Option<&Path>,
    metadata: &RecordingMeta,
    partial: bool,
) -> Result<()> {
    if !writer.has_video() {
        return Ok(());
    }
    if let Some(output) = output {
        match writer.finish(output, metadata, partial).await {
            Ok(path) => println!(
                "Wrote {}recording to {}",
                if partial { "partial " } else { "" },
                path.display()
            ),
            Err(err) => {
                // Keep the spool on disk if finalization fails; callers report its location.
                return Err(err).context("Could not finalize recording; spool retained");
            }
        }
    } else {
        log::info!(
            "Replay: received {} video frames, {} raw bytes",
            writer.frames,
            writer.raw_bytes
        );
    }
    Ok(())
}

fn retry_download_error(error: &anyhow::Error) -> bool {
    match error.downcast_ref::<neolink_core::Error>() {
        Some(neolink_core::Error::CameraServiceUnavailable {
            code: 401 | 403, ..
        }) => false,
        Some(neolink_core::Error::Other(
            "Replay interrupted by user" | "Explicit replay duration limit reached",
        )) => false,
        Some(_) => true,
        None => false, // Output I/O failures must not start another camera download.
    }
}

/// Playback retains the legacy metadata timer. A download uses transport completion instead.
fn replay_duration(
    explicit: Option<u64>,
    is_download: bool,
    metadata: Option<&FileInfo>,
) -> Option<u64> {
    if explicit.is_some() || is_download {
        return explicit;
    }
    let metadata = metadata?;
    let convert = |date: &ReplayDateTime| {
        Some(time::PrimitiveDateTime::new(
            time::Date::from_calendar_date(date.year, date.month.try_into().ok()?, date.day)
                .ok()?,
            time::Time::from_hms(date.hour, date.minute, date.second).ok()?,
        ))
    };
    let start = convert(metadata.start_time.as_ref()?)?;
    let end = convert(metadata.end_time.as_ref()?)?;
    let seconds = (end - start).whole_seconds();
    (seconds > 0).then_some(seconds as u64)
}

async fn run_replay_or_download(
    camera: &crate::common::NeoInstance,
    name: &str,
    stream_type: &str,
    speed: u32,
    output: Option<std::path::PathBuf>,
    duration: Option<u64>,
    dump_replay: Option<std::path::PathBuf>,
    dump_replay_limit: Option<usize>,
    is_download: bool,
) -> Result<()> {
    // Metadata is optional and bounded. ID 13 is queried by start_replay after seek (123).
    let file_meta = tokio::time::timeout(
        Duration::from_secs(20),
        camera.run_task(|cam| {
            let name = name.to_string();
            let stream_type = stream_type.to_string();
            Box::pin(async move {
                Ok(cam
                    .get_replay_file_metadata(
                        &name,
                        &stream_type,
                        cmdline::FILE_SEARCH_RECORD_TYPES,
                    )
                    .await
                    .ok()
                    .flatten())
            })
        }),
    )
    .await
    .ok()
    .and_then(Result::ok)
    .flatten();
    let duration = replay_duration(duration, is_download, file_meta.as_ref());
    let expected_size = file_meta.as_ref().and_then(|info| {
        let size = info.size_l.unwrap_or(0) as u64 + ((info.size_h.unwrap_or(0) as u64) << 32);
        (size > 0).then_some(size)
    });
    let metadata = RecordingMeta {
        record_type: file_meta.as_ref().and_then(|f| f.record_type.clone()),
        start_time: file_meta
            .as_ref()
            .and_then(|f| f.start_time.as_ref().map(format_replay_datetime)),
        end_time: file_meta
            .as_ref()
            .and_then(|f| f.end_time.as_ref().map(format_replay_datetime)),
        camera_name: None,
    };
    let time_range = if is_download {
        file_meta
            .as_ref()
            .and_then(|f| Some((f.start_time.clone()?, f.end_time.clone()?)))
    } else {
        None
    };
    let mut use_time_range = time_range.is_some();
    loop {
        let mut writer = output::RecordingWriter::new(output.as_deref())?;
        if output.is_none() {
            writer.discard_payloads();
        }
        let started = camera
            .run_task(|cam| {
                let name = name.to_string();
                let stream_type = stream_type.to_string();
                let dump = dump_replay.clone();
                let time_range = time_range.clone();
                Box::pin(async move {
                    if use_time_range {
                        let (start, end) = time_range.unwrap();
                        let stream = stream_number(&stream_type)?;
                        cam.start_download_by_time(
                            start,
                            end,
                            "",
                            stream,
                            false,
                            100,
                            dump,
                            dump_replay_limit,
                        )
                        .await
                        .map_err(anyhow::Error::from)
                    } else {
                        cam.start_replay(
                            &name,
                            &stream_type,
                            speed,
                            false,
                            100,
                            dump,
                            dump_replay_limit,
                            expected_size,
                        )
                        .await
                        .map_err(anyhow::Error::from)
                    }
                })
            })
            .await;
        let mut stream = match started {
            Ok(stream) => stream,
            Err(err) if use_time_range && retry_download_error(&err) => {
                log::warn!(
                    "Download: MSG 143 start failed: {}; trying legacy replay",
                    err
                );
                writer.close();
                use_time_range = false;
                continue;
            }
            Err(err) => {
                writer.close();
                return Err(err);
            }
        };
        let mut result = receive_recording(&mut stream, &mut writer, duration).await;
        let cleanup = stop_recording(
            camera,
            &mut stream,
            if use_time_range { None } else { Some(name) },
        )
        .await;
        if let Err(err) = cleanup {
            if result.is_ok() {
                result = Err(err);
            }
        }
        if matches!(result, Ok(true)) && is_download && !writer.has_video() {
            result = Err(neolink_core::Error::Other("Camera ended download without video").into());
        }
        let retry = use_time_range && result.as_ref().err().map_or(false, retry_download_error);
        // An accepted but failed ID 143 attempt has a distinct partial result; never append the replay retry.
        let attempt_output = if retry {
            output.as_ref().map(|p| {
                p.with_file_name(format!(
                    "{}.id143.{}",
                    p.file_stem().unwrap_or_default().to_string_lossy(),
                    p.extension().unwrap_or_default().to_string_lossy()
                ))
            })
        } else {
            output.clone()
        };
        let saved = save_recording(
            &mut writer,
            attempt_output.as_deref(),
            &metadata,
            !matches!(result, Ok(true)),
        )
        .await;
        if saved.is_err() {
            log::error!(
                "Recording spool retained at {}",
                writer.directory().display()
            );
        } else {
            writer.close();
        }
        saved?;
        if retry {
            log::warn!(
                "Download: MSG 143 receive failed: {}; trying legacy replay",
                result.unwrap_err()
            );
            use_time_range = false;
            continue;
        }
        return result.map(|_| ());
    }
}

fn stream_number(stream: &str) -> Result<u32> {
    match stream {
        "mainStream" => Ok(0),
        "subStream" => Ok(1),
        "externStream" => Ok(2),
        other => anyhow::bail!("Unknown stream type: {}", other),
    }
}

async fn run_download_by_time(
    camera: &crate::common::NeoInstance,
    start_time: ReplayDateTime,
    end_time: ReplayDateTime,
    stream_type: &str,
    output: std::path::PathBuf,
    duration: Option<u64>,
    dump_replay: Option<std::path::PathBuf>,
    dump_replay_limit: Option<usize>,
) -> Result<()> {
    let stream_type = stream_number(stream_type)?;
    let metadata = RecordingMeta {
        start_time: Some(format_replay_datetime(&start_time)),
        end_time: Some(format_replay_datetime(&end_time)),
        ..Default::default()
    };
    let mut writer = output::RecordingWriter::new(Some(&output))?;
    let started = camera
        .run_task(|cam| {
            let start_time = start_time.clone();
            let end_time = end_time.clone();
            let dump = dump_replay.clone();
            Box::pin(async move {
                cam.start_download_by_time(
                    start_time,
                    end_time,
                    "",
                    stream_type,
                    false,
                    100,
                    dump,
                    dump_replay_limit,
                )
                .await
                .map_err(anyhow::Error::from)
            })
        })
        .await;
    let mut stream = match started {
        Ok(stream) => stream,
        Err(err) => {
            writer.close();
            return Err(err);
        }
    };
    let mut result = receive_recording(&mut stream, &mut writer, duration).await;
    let cleanup = stop_recording(camera, &mut stream, None).await;
    if let Err(err) = cleanup {
        if result.is_ok() {
            result = Err(err);
        }
    }
    if matches!(result, Ok(true)) && !writer.has_video() {
        result = Err(anyhow::anyhow!("Camera ended download without video"));
    }
    let saved = save_recording(
        &mut writer,
        Some(&output),
        &metadata,
        !matches!(result, Ok(true)),
    )
    .await;
    if saved.is_err() {
        log::error!(
            "Recording spool retained at {}",
            writer.directory().display()
        );
    } else {
        writer.close();
    }
    saved?;
    result.map(|_| ())
}

/// Entry point for the replay subcommand
pub(crate) async fn main(opt: Opt, reactor: NeoReactor) -> Result<()> {
    let camera = reactor.get(&opt.camera).await?;

    match opt.cmd {
        cmdline::ReplayCommand::Days { start, end } => {
            let (sy, sm, sd) = parse_date(&start)?;
            let end_str = end.as_deref().unwrap_or(&start);
            let (ey, em, ed) = parse_date(end_str)?;
            let start_time = date_to_replay_start(sy, sm, sd);
            let end_time = date_to_replay_end(ey, em, ed);

            let records = camera
                .run_task(|cam| {
                    let st = start_time.clone();
                    let et = end_time.clone();
                    Box::pin(async move {
                        cam.get_day_records(st, et)
                            .await
                            .context("Could not get day records from camera")
                    })
                })
                .await?;

            let single_day = start_time.year == end_time.year
                && start_time.month == end_time.month
                && start_time.day == end_time.day;
            if records.day_type_list.is_none() && single_day {
                // E1 and some cameras don't send dayTypeList; fall back to file list for this day
                let handle_info = camera
                    .run_task({
                        let st = start_time.clone();
                        let et = end_time.clone();
                        move |cam| {
                            let st = st.clone();
                            let et = et.clone();
                            Box::pin(async move {
                                cam.get_file_list_handle(
                                    "subStream",
                                    cmdline::FILE_SEARCH_RECORD_TYPES,
                                    st,
                                    et,
                                )
                                .await
                                .context("Could not get file list handle")
                            })
                        }
                    })
                    .await?;
                if let Some(handle) = handle_info.handle {
                    let files = camera
                        .run_task(|cam| {
                            Box::pin(async move {
                                cam.get_file_list_by_handle(handle)
                                    .await
                                    .context("Could not get file list")
                            })
                        })
                        .await?;
                    let n = files.len();
                    if n == 0 {
                        println!("No recordings for this day.");
                    } else {
                        println!("This day has {} recording(s).", n);
                    }
                } else {
                    print_day_records(&records);
                }
            } else {
                print_day_records(&records);
            }
        }
        cmdline::ReplayCommand::Files {
            date,
            stream,
            record_type,
            ai_filter,
        } => {
            let (y, m, d) = parse_date(&date)?;
            let start_time = date_to_replay_start(y, m, d);
            let end_time = date_to_replay_end(y, m, d);

            let handle_result = camera
                .run_task(|cam| {
                    let st = start_time.clone();
                    let et = end_time.clone();
                    let stream = stream.clone();
                    let record_type = record_type.clone();
                    Box::pin(async move {
                        cam.get_file_list_handle(&stream, &record_type, st, et)
                            .await
                            .map_err(|e| anyhow::anyhow!("{}", e))
                    })
                })
                .await;

            // E1 cameras return 400 when no recordings exist for the date
            let handle_info = match handle_result {
                Ok(info) => info,
                Err(e) => {
                    let msg = format!("{}", e);
                    if msg.contains("returned code 400") {
                        println!("No files for this day.");
                        return Ok(());
                    }
                    return Err(e).context("Could not get file list handle from camera");
                }
            };

            let handle = match handle_info.handle {
                Some(h) => h,
                None => anyhow::bail!("Camera did not return a file list handle"),
            };

            let files = camera
                .run_task(|cam| {
                    Box::pin(async move {
                        cam.get_file_list_by_handle(handle)
                            .await
                            .context("Could not get file list from camera")
                    })
                })
                .await?;

            // Apply --ai-filter: only show files whose recordType contains at least one requested tag
            let files = if let Some(ref filter) = ai_filter {
                let wanted: Vec<&str> = filter.split(',').map(|s| s.trim()).collect();
                files
                    .into_iter()
                    .filter(|f| {
                        if let Some(ref rt) = f.record_type {
                            let tags: Vec<&str> = rt.split(',').map(|s| s.trim()).collect();
                            wanted.iter().any(|w| tags.contains(w))
                        } else {
                            false
                        }
                    })
                    .collect()
            } else {
                files
            };

            print_file_list(&files);
        }
        cmdline::ReplayCommand::Play {
            name,
            stream: stream_type,
            speed,
            output,
            duration,
            dump_replay,
            dump_replay_limit,
        } => {
            run_replay_or_download(
                &camera,
                &name,
                &stream_type,
                speed,
                output,
                duration,
                dump_replay,
                dump_replay_limit,
                false,
            )
            .await?;
        }
        cmdline::ReplayCommand::Download {
            name,
            stream: stream_type,
            output,
            duration,
            dump_replay,
            dump_replay_limit,
        } => {
            log::info!(
                "Replay download: name={} stream={} (MSG 143; stops on camera end response or explicit --duration)",
                name,
                stream_type
            );
            let output =
                output.ok_or_else(|| anyhow::anyhow!("Download requires --output <file>"))?;
            run_replay_or_download(
                &camera,
                &name,
                &stream_type,
                1,
                Some(output),
                duration,
                dump_replay,
                dump_replay_limit,
                true,
            )
            .await?;
        }
        cmdline::ReplayCommand::DownloadByTime {
            start,
            end,
            stream: stream_type,
            output,
            duration,
            dump_replay,
            dump_replay_limit,
        } => {
            let output = output
                .ok_or_else(|| anyhow::anyhow!("download-by-time requires --output <file>"))?;
            let (sy, sm, sd) = parse_date(&start)?;
            let end_str = end.as_deref().unwrap_or(&start);
            let (ey, em, ed) = parse_date(end_str)?;
            let start_time = date_to_replay_start(sy, sm, sd);
            let end_time = date_to_replay_end(ey, em, ed);
            run_download_by_time(
                &camera,
                start_time,
                end_time,
                &stream_type,
                output,
                duration,
                dump_replay,
                dump_replay_limit,
            )
            .await?;
        }
        cmdline::ReplayCommand::AlarmSearch {
            start,
            end,
            stream_type,
            alarm_types,
        } => {
            let (sy, sm, sd) = parse_date(&start)?;
            let end_str = end.as_deref().unwrap_or(&start);
            let (ey, em, ed) = parse_date(end_str)?;
            let start_time = date_to_replay_start(sy, sm, sd);
            let end_time = date_to_replay_end(ey, em, ed);

            let alarm_list: Vec<String> = alarm_types
                .split(',')
                .map(|s| s.trim().to_string())
                .collect();
            let alarm_refs: Vec<&str> = alarm_list.iter().map(|s| s.as_str()).collect();

            // START: send search params, get handle
            let result = camera
                .run_task(|cam| {
                    let st = start_time.clone();
                    let et = end_time.clone();
                    let ar: Vec<String> = alarm_refs.iter().map(|s| s.to_string()).collect();
                    Box::pin(async move {
                        let refs: Vec<&str> = ar.iter().map(|s| s.as_str()).collect();
                        cam.alarm_video_search_start(stream_type, &refs, st, et)
                            .await
                            .map_err(|e| anyhow::anyhow!("{}", e))
                    })
                })
                .await;

            // Handle cameras that don't support alarm search (405 = not supported)
            let result = match result {
                Ok(r) => r,
                Err(e) => {
                    let msg = format!("{}", e);
                    if msg.contains("returned code 405") {
                        println!("This camera does not support alarm video search (MSG 175).");
                        return Ok(());
                    }
                    if msg.contains("returned code 400") {
                        println!("No alarm events found for this date range.");
                        return Ok(());
                    }
                    return Err(e).context("Alarm video search failed");
                }
            };

            println!("Alarm search response:");
            println!("  channelId:  {:?}", result.channel_id);
            println!("  fileHandle: {:?}", result.file_handle);
            println!("  streamType: {:?}", result.stream_type);
            println!("  alarmType:  {:?}", result.alarm_type);
            if let Some(ref st) = result.start_time {
                println!("  startTime:  {}", format_replay_datetime(st));
            }
            if let Some(ref et) = result.end_time {
                println!("  endTime:    {}", format_replay_datetime(et));
            }

            // If we got a handle, paginate to collect all events
            if let Some(handle) = result.file_handle {
                if handle >= 0 {
                    println!("\nPaginating with handle {}...", handle);
                    let mut page = 0;
                    loop {
                        page += 1;
                        let next_result = camera
                            .run_task(|cam| {
                                Box::pin(async move {
                                    cam.alarm_video_search_next(handle)
                                        .await
                                        .context("Alarm video search DO/paginate failed")
                                })
                            })
                            .await;
                        match next_result {
                            Ok(fav) => {
                                println!("\n--- Page {} ---", page);
                                println!("  channelId:  {:?}", fav.channel_id);
                                println!("  fileHandle: {:?}", fav.file_handle);
                                println!("  alarmType:  {:?}", fav.alarm_type);
                                if let Some(ref st) = fav.start_time {
                                    println!("  startTime:  {}", format_replay_datetime(st));
                                }
                                if let Some(ref et) = fav.end_time {
                                    println!("  endTime:    {}", format_replay_datetime(et));
                                }
                                // If no more data or handle changes, stop
                                if fav.file_handle.is_none() || fav.file_handle == Some(-1) {
                                    println!("\nEnd of alarm search results.");
                                    break;
                                }
                            }
                            Err(e) => {
                                // Camera returns non-200 when no more results
                                log::debug!("Alarm search pagination ended: {}", e);
                                println!("\nEnd of alarm search results (page {}).", page);
                                break;
                            }
                        }
                        if page >= 100 {
                            println!("Reached page limit (100). Stopping.");
                            break;
                        }
                    }
                }
            }
        }
        cmdline::ReplayCommand::Stop { name } => {
            camera
                .run_task(|cam| {
                    let name = name.clone();
                    Box::pin(async move {
                        cam.replay_stop(&name)
                            .await
                            .context("Could not send replay stop to camera")
                    })
                })
                .await?;
            println!("Replay stop sent.");
        }
    }

    // Force exit to ensure the process terminates even if background tasks keep the runtime alive.
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(0);
}

fn print_day_records(records: &DayRecords) {
    if let Some(ref list) = records.day_type_list {
        if list.day_type.is_empty() {
            println!("No recording days in range.");
            return;
        }
        println!("Days with recordings:");
        for dt in &list.day_type {
            // index is day-in-range (0 = first day). type is e.g. "normal"
            println!("  Day index {}: {}", dt.index, dt.type_);
        }
    } else {
        println!(
            "This camera does not report which days have recordings.\n\
             Use: neolink replay <camera> files --date YYYY-MM-DD to list files for a specific day."
        );
    }
}

/// AI detection tag names from recordType.
const AI_TAGS: &[&str] = &[
    "people",
    "vehicle",
    "face",
    "dog_cat",
    "package",
    "visitor",
    "cry",
    "crossline",
    "intrusion",
    "loitering",
    "nonmotorveh",
];

fn print_file_list(files: &[FileInfo]) {
    if files.is_empty() {
        println!("No files for this day.");
        return;
    }
    println!("Files ({}):", files.len());
    for f in files {
        let name = f.name.as_deref().unwrap_or("—");
        let size_l = f.size_l.unwrap_or(0);
        let size_h = f.size_h.unwrap_or(0);
        let size_bytes = size_l as u64 + ((size_h as u64) << 32);
        let start = f
            .start_time
            .as_ref()
            .map(|t| {
                format!(
                    "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
                    t.year, t.month, t.day, t.hour, t.minute, t.second
                )
            })
            .unwrap_or_else(|| "—".to_string());
        let end = f
            .end_time
            .as_ref()
            .map(|t| {
                format!(
                    "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
                    t.year, t.month, t.day, t.hour, t.minute, t.second
                )
            })
            .unwrap_or_else(|| "—".to_string());
        let stream = f.stream_type.as_deref().unwrap_or("—");
        let rec_type = f.record_type.as_deref().unwrap_or("");
        // Split recordType into trigger (md/sched/manual/pir/io) and AI detections
        let ai: Vec<&str> = rec_type
            .split(',')
            .map(|s| s.trim())
            .filter(|s| AI_TAGS.contains(s))
            .collect();
        let ai_str = if ai.is_empty() {
            String::new()
        } else {
            format!("  [AI: {}]", ai.join(", "))
        };
        let size_str = if size_bytes == 0 {
            "— ".to_string()
        } else if size_bytes < 1024 * 1024 {
            format!("{} KB", size_bytes / 1024)
        } else {
            format!("{} MB", size_bytes / (1024 * 1024))
        };
        println!(
            "  {}  {}  {}  {}  {}  {}{}",
            name, size_str, start, end, stream, rec_type, ai_str
        );
    }
}
