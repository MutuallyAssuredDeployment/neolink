//! Disk-backed recording. Memory use does not grow with recording length.
use super::{Context, RecordingMeta, Result};
use neolink_core::bcmedia::model::{BcMedia, ReplayFormat, VideoType};
use std::{
    fs::File,
    io::{BufReader, Read, Write},
    path::{Path, PathBuf},
};

pub(super) struct RecordingWriter {
    directory: PathBuf,
    store: bool,
    video: File,
    index: File,
    audio: File,
    raw: File,
    pub(super) frames: u64,
    pub(super) raw_bytes: u64,
    audio_bytes: u64,
    format: ReplayFormat,
    video_type: VideoType,
    fps: u8,
    last_ts: Option<u32>,
    total_delta: u64,
    deltas: u64,
}

/// One frame at a time, with a cumulative timestamp that also handles multiple u32 wraps.
pub(super) struct FrameReader {
    video: BufReader<File>,
    index: BufReader<File>,
    last_ts: Option<u32>,
    pts_us: u64,
}
impl FrameReader {
    pub(super) fn open(directory: &Path) -> Result<Self> {
        Ok(Self {
            video: BufReader::new(File::open(directory.join("video"))?),
            index: BufReader::new(File::open(directory.join("index"))?),
            last_ts: None,
            pts_us: 0,
        })
    }
    pub(super) fn next(&mut self) -> Result<Option<(Vec<u8>, u64)>> {
        let mut entry = [0; 8];
        loop {
            if self.index.read(&mut entry[..1])? == 0 {
                return Ok(None);
            }
            self.index.read_exact(&mut entry[1..])?;
            let size = u32::from_le_bytes(entry[..4].try_into().unwrap()) as usize;
            let ts = u32::from_le_bytes(entry[4..].try_into().unwrap());
            if size == 0 {
                self.last_ts = None;
                self.pts_us += ts as u64;
                continue;
            }
            anyhow::ensure!(
                size <= 64 * 1024 * 1024,
                "Invalid recording index frame size"
            );
            if let Some(last) = self.last_ts {
                let delta = ts.wrapping_sub(last);
                // Section overlap may reset timestamps. Keep output PTS monotonic.
                self.pts_us += if delta <= u32::MAX / 2 {
                    delta as u64
                } else {
                    1
                };
            }
            self.last_ts = Some(ts);
            let mut data = vec![0; size];
            self.video.read_exact(&mut data)?;
            return Ok(Some((data, self.pts_us)));
        }
    }
}
use std::convert::TryInto;

impl RecordingWriter {
    pub(super) fn new(output: Option<&Path>) -> Result<Self> {
        let parent = output
            .and_then(Path::parent)
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir);
        let directory = parent.join(format!(".neolink-replay-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).context("Create recording spool")?;
        let files = (|| -> Result<_> {
            Ok((
                File::create(directory.join("video"))?,
                File::create(directory.join("index"))?,
                File::create(directory.join("audio"))?,
                File::create(directory.join("raw"))?,
            ))
        })();
        let (video, index, audio, raw) = match files {
            Ok(files) => files,
            Err(err) => {
                let _ = std::fs::remove_dir_all(&directory);
                return Err(err);
            }
        };
        Ok(Self {
            directory,
            store: true,
            video,
            index,
            audio,
            raw,
            frames: 0,
            raw_bytes: 0,
            audio_bytes: 0,
            format: ReplayFormat::BcMedia,
            video_type: VideoType::H264,
            fps: 25,
            last_ts: None,
            total_delta: 0,
            deltas: 0,
        })
    }

    pub(super) fn discard_payloads(&mut self) {
        self.store = false;
    }

    pub(super) fn push(&mut self, frame: BcMedia) -> Result<()> {
        match frame {
            BcMedia::Iframe(frame) => {
                self.write_frame(&frame.data, frame.microseconds, frame.video_type)?
            }
            BcMedia::Pframe(frame) => {
                self.write_frame(&frame.data, frame.microseconds, frame.video_type)?
            }
            BcMedia::Aac(frame) => {
                if self.store {
                    self.audio.write_all(&frame.data)?;
                }
                self.audio_bytes += frame.data.len() as u64;
            }
            BcMedia::InfoV1(info) => {
                if info.fps > 0 {
                    self.fps = info.fps;
                }
            }
            BcMedia::InfoV2(info) => {
                if info.fps > 0 {
                    self.fps = info.fps;
                }
            }
            BcMedia::ReplayFormat(format) => self.format = format,
            BcMedia::RawReplayChunk(data) => {
                if self.store {
                    self.raw.write_all(&data)?;
                }
                self.raw_bytes += data.len() as u64;
            }
            BcMedia::ReplaySection => {
                if self.store {
                    self.index.write_all(&0u32.to_le_bytes())?;
                    self.index
                        .write_all(&(1_000_000u32 / self.fps.max(1) as u32).to_le_bytes())?;
                }
                self.last_ts = None;
                log::info!("Replay: receiving next recording section");
            }
            _ => {}
        }
        Ok(())
    }
    fn write_frame(&mut self, data: &[u8], ts: u32, video_type: VideoType) -> Result<()> {
        let has_start = data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1]);
        let len = data.len() + if has_start { 0 } else { 4 };
        let len = u32::try_from(len).context("Recording frame too large")?;
        if self.store {
            if !has_start {
                self.video.write_all(&[0, 0, 0, 1])?;
            }
            self.video.write_all(data)?;
            self.index.write_all(&len.to_le_bytes())?;
            self.index.write_all(&ts.to_le_bytes())?;
        }
        if let Some(last) = self.last_ts {
            let delta = ts.wrapping_sub(last);
            if delta > 0 && delta < 2_000_000 {
                self.total_delta += delta as u64;
                self.deltas += 1;
            }
        }
        self.last_ts = Some(ts);
        self.video_type = video_type;
        self.frames += 1;
        Ok(())
    }

    pub(super) fn directory(&self) -> &Path {
        &self.directory
    }

    pub(super) fn has_video(&self) -> bool {
        self.frames > 0 || self.raw_bytes > 0
    }

    /// Finalize only after protocol cleanup; errors use an explicitly partial output name.
    pub(super) async fn finish(
        &mut self,
        output: &Path,
        meta: &RecordingMeta,
        partial: bool,
    ) -> Result<PathBuf> {
        self.video.flush()?;
        self.index.flush()?;
        self.audio.flush()?;
        self.raw.flush()?;
        let output = if partial {
            partial_path(output)
        } else {
            output.to_path_buf()
        };
        let is_mp4 = output
            .extension()
            .and_then(|ext| ext.to_str())
            .map_or(false, |ext| ext.eq_ignore_ascii_case("mp4"));
        if self.raw_bytes > 0 {
            match self.format {
                ReplayFormat::Mp4 => {
                    let source = self.directory.join("raw");
                    let dest = output.clone();
                    match tokio::task::spawn_blocking(move || {
                        super::container::write_container(&source, &dest)
                    })
                    .await
                    {
                        Ok(Ok(())) => {}
                        error => {
                            log::warn!(
                                "Replay: incomplete/unsupported MP4: {:?}; retaining raw container",
                                error
                            );
                            let _ = tokio::fs::remove_file(&output).await;
                            let raw_output = output.with_extension("replay.bin");
                            tokio::fs::copy(self.directory.join("raw"), &raw_output).await?;
                            return Ok(raw_output);
                        }
                    }
                    super::patch_e1_avcc_if_needed(&output)?;
                    super::patch_avc1_colour_if_needed(&output)?;
                    return Ok(output);
                }
                ReplayFormat::AnnexB | ReplayFormat::H264Nal => {
                    let source = self.directory.join("raw");
                    if is_mp4 {
                        // Annex B does not carry BcMedia timestamps. Keep the legacy FPS fallback.
                        if self.ffmpeg(&source, &output, meta).await.unwrap_or(false) {
                            return Ok(output);
                        }
                        let _ = tokio::fs::remove_file(&output).await;
                    }
                    let output = if is_mp4 {
                        output.with_extension("h264")
                    } else {
                        output
                    };
                    tokio::fs::copy(source, &output).await?;
                    return Ok(output);
                }
                _ => {
                    let output = if is_mp4 {
                        output.with_extension("replay.bin")
                    } else {
                        output
                    };
                    tokio::fs::copy(self.directory.join("raw"), &output).await?;
                    return Ok(output);
                }
            }
        }
        if is_mp4 {
            #[cfg(feature = "gstreamer")]
            {
                let directory = self.directory.clone();
                let path = output.clone();
                let video_type = self.video_type;
                let has_audio = self.audio_bytes > 0;
                let metadata = super::gst::Mp4Metadata {
                    record_type: meta.record_type.clone(),
                    start_time: meta.start_time.clone(),
                    end_time: meta.end_time.clone(),
                    camera_name: meta.camera_name.clone(),
                };
                match tokio::task::spawn_blocking(move || {
                    super::gst::mux_recording_to_mp4(
                        &directory, video_type, has_audio, &path, &metadata,
                    )
                })
                .await
                {
                    Ok(Ok(())) => return Ok(output),
                    err => log::warn!("Replay: GStreamer mux failed: {:?}; trying ffmpeg", err),
                }
            }
            if self
                .ffmpeg(&self.directory.join("video"), &output, meta)
                .await
                .unwrap_or(false)
            {
                return Ok(output);
            }
            // A failed mux can leave an invalid MP4. Keep elementary video and audio instead.
            let _ = tokio::fs::remove_file(&output).await;
        }
        let extension = match self.video_type {
            VideoType::H264 => "h264",
            VideoType::H265 => "h265",
        };
        let output = if is_mp4 {
            output.with_extension(extension)
        } else {
            output
        };
        tokio::fs::copy(self.directory.join("video"), &output).await?;
        if self.audio_bytes > 0 {
            tokio::fs::copy(self.directory.join("audio"), output.with_extension("aac")).await?;
        }
        Ok(output)
    }

    async fn ffmpeg(&self, video: &Path, output: &Path, meta: &RecordingMeta) -> Result<bool> {
        let average = if self.deltas > 0 && self.total_delta > 0 {
            1_000_000.0 * self.deltas as f64 / self.total_delta as f64
        } else {
            self.fps as f64
        };
        let fps = if (1.0..=120.0).contains(&average) {
            average
        } else {
            self.fps as f64
        };
        let mut cmd = tokio::process::Command::new("ffmpeg");
        let format = match self.video_type {
            VideoType::H264 => "h264",
            VideoType::H265 => "hevc",
        };
        cmd.args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-fflags",
            "+genpts",
            "-r",
            &format!("{:.3}", fps),
            "-f",
            format,
            "-i",
        ])
        .arg(video);
        if self.audio_bytes > 0 {
            cmd.args(["-f", "aac", "-i"])
                .arg(self.directory.join("audio"));
        }
        cmd.args(["-c", "copy", "-movflags", "+faststart"]);
        if let Some(rt) = &meta.record_type {
            cmd.args(["-metadata", &format!("comment=recordType: {}", rt)]);
        }
        let description = vec![
            meta.start_time.as_ref().map(|s| format!("Start: {}", s)),
            meta.end_time.as_ref().map(|s| format!("End: {}", s)),
            meta.camera_name.as_ref().map(|s| format!("Camera: {}", s)),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("; ");
        if !description.is_empty() {
            cmd.args(["-metadata", &format!("description={}", description)]);
        }
        let mut child = cmd
            .arg(output)
            .kill_on_drop(true)
            .spawn()
            .context("Run ffmpeg")?;
        let result = tokio::time::timeout(std::time::Duration::from_secs(120), child.wait()).await;
        Ok(result.context("ffmpeg mux timed out")??.success())
    }
}
use std::convert::TryFrom;

impl RecordingWriter {
    pub(super) fn close(self) {
        let Self {
            directory,
            video,
            index,
            audio,
            raw,
            ..
        } = self;
        drop((video, index, audio, raw));
        if let Err(err) = std::fs::remove_dir_all(&directory) {
            log::warn!(
                "Could not remove recording spool {}: {}",
                directory.display(),
                err
            );
        }
    }
}

pub(super) fn partial_path(output: &Path) -> PathBuf {
    let name = output.file_stem().unwrap_or_default().to_string_lossy();
    let extension = output.extension().unwrap_or_default().to_string_lossy();
    output.with_file_name(if extension.is_empty() {
        format!("{}.partial", name)
    } else {
        format!("{}.partial.{}", name, extension)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use neolink_core::bcmedia::model::BcMediaPframe;
    #[tokio::test]
    async fn spools_frames_and_keeps_partial_video_and_audio() {
        let mut writer = RecordingWriter::new(None).unwrap();
        let directory = writer.directory.clone();
        for ts in [u32::MAX - 10, 20, 60] {
            writer
                .push(BcMedia::Pframe(BcMediaPframe {
                    video_type: VideoType::H264,
                    data: vec![0x41, 0xaa],
                    microseconds: ts,
                }))
                .unwrap();
        }
        writer
            .push(BcMedia::Aac(neolink_core::bcmedia::model::BcMediaAac {
                data: vec![0xff, 0xf1, 0x60, 0x40, 0, 0xff, 0xfc],
            }))
            .unwrap();
        let mut reader = FrameReader::open(&directory).unwrap();
        assert_eq!(
            reader.next().unwrap().unwrap(),
            (vec![0, 0, 0, 1, 0x41, 0xaa], 0)
        );
        assert_eq!(reader.next().unwrap().unwrap().1, 31);
        assert_eq!(reader.next().unwrap().unwrap().1, 71);
        assert!(reader.next().unwrap().is_none());
        drop(reader);
        let destination = directory.join("test.h264");
        let partial = writer
            .finish(&destination, &RecordingMeta::default(), true)
            .await
            .unwrap();
        assert_eq!(std::fs::metadata(&partial).unwrap().len(), 18);
        assert!(partial.ends_with("test.partial.h264"));
        assert_eq!(
            std::fs::metadata(partial.with_extension("aac"))
                .unwrap()
                .len(),
            7
        );
        writer.close();
        assert!(!directory.exists());
    }

    #[test]
    fn section_reset_and_slow_frame_intervals_keep_monotonic_pts() {
        let mut writer = RecordingWriter::new(None).unwrap();
        for frame in [
            BcMedia::Pframe(BcMediaPframe {
                video_type: VideoType::H264,
                data: vec![0x41; 10],
                microseconds: 1_000_000,
            }),
            BcMedia::Pframe(BcMediaPframe {
                video_type: VideoType::H264,
                data: vec![0x41; 10],
                microseconds: 5_000_000,
            }),
            BcMedia::ReplaySection,
            BcMedia::Pframe(BcMediaPframe {
                video_type: VideoType::H264,
                data: vec![0x41; 10],
                microseconds: 1_000,
            }),
        ] {
            writer.push(frame).unwrap();
        }
        let mut reader = FrameReader::open(writer.directory()).unwrap();
        assert_eq!(reader.next().unwrap().unwrap().1, 0);
        assert_eq!(reader.next().unwrap().unwrap().1, 4_000_000);
        assert_eq!(reader.next().unwrap().unwrap().1, 4_040_000);
        drop(reader);
        writer.close();
    }
}
