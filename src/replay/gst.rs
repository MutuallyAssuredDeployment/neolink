//! GStreamer pipeline to mux H.264/H.265 + optional AAC audio to MP4.
//! Supports per-frame timestamps (VFR) via appsrc PTS.
//! Requires the `gstreamer` feature.

use anyhow::{Context, Result};
use gstreamer::prelude::*;
use gstreamer::{Caps, ClockTime, MessageView, Pipeline, State};
use gstreamer_app::AppSrc;
use neolink_core::bcmedia::model::VideoType;
use std::path::Path;

/// Optional metadata to embed in the MP4 container.
#[derive(Debug, Default, Clone)]
pub struct Mp4Metadata {
    /// Camera recording type / AI detection tags (e.g. "manual,sched,md,people,vehicle,dog_cat").
    /// Stored as MP4 comment + keywords.
    pub record_type: Option<String>,
    /// Recording start time as "YYYY-MM-DD HH:MM:SS".
    pub start_time: Option<String>,
    /// Recording end time as "YYYY-MM-DD HH:MM:SS".
    pub end_time: Option<String>,
    /// Camera name / channel.
    pub camera_name: Option<String>,
}

/// Feed disk-backed video and audio concurrently with bounded appsrc queues.
pub fn mux_recording_to_mp4(
    directory: &Path,
    video_type: VideoType,
    has_audio: bool,
    output_path: &Path,
    metadata: &Mp4Metadata,
) -> Result<()> {
    gstreamer::init().context("GStreamer init")?;

    let pipeline = Pipeline::new();

    // --- Video path ---
    let video_src = gstreamer::ElementFactory::make("appsrc")
        .build()
        .context("create video appsrc")?
        .downcast::<AppSrc>()
        .map_err(|_| anyhow::anyhow!("appsrc downcast"))?;
    let (parser_name, caps_name) = match video_type {
        VideoType::H264 => ("h264parse", "video/x-h264"),
        VideoType::H265 => ("h265parse", "video/x-h265"),
    };
    let video_parse = gstreamer::ElementFactory::make(parser_name)
        .build()
        .with_context(|| format!("create {}", parser_name))?;
    let mp4mux = gstreamer::ElementFactory::make("mp4mux")
        .build()
        .context("create mp4mux")?;
    mp4mux.set_property("faststart", true);
    let filesink = gstreamer::ElementFactory::make("filesink")
        .build()
        .context("create filesink")?;
    filesink.set_property("location", output_path);

    let video_src_el = video_src.upcast_ref();
    pipeline.add_many([video_src_el, &video_parse, &mp4mux, &filesink])?;
    video_src_el.link(&video_parse)?;
    video_parse.link(&mp4mux)?;
    mp4mux.link(&filesink)?;

    let caps = Caps::builder(caps_name)
        .field("stream-format", "byte-stream")
        .build();
    video_src.set_caps(Some(&caps));
    video_src.set_stream_type(gstreamer_app::AppStreamType::Stream);
    video_src.set_format(gstreamer::Format::Time);
    video_src.set_max_bytes(1024 * 1024);
    video_src.set_block(true);

    // --- Audio path (optional) ---
    let audio_src = if has_audio {
        let src = gstreamer::ElementFactory::make("appsrc")
            .build()
            .context("create audio appsrc")?
            .downcast::<AppSrc>()
            .map_err(|_| anyhow::anyhow!("audio appsrc downcast"))?;
        let aacparse = gstreamer::ElementFactory::make("aacparse")
            .build()
            .context("create aacparse")?;

        let src_el = src.upcast_ref();
        pipeline.add_many([src_el, &aacparse])?;
        src_el.link(&aacparse)?;
        aacparse.link(&mp4mux)?;

        let caps = Caps::builder("audio/mpeg")
            .field("mpegversion", 4i32)
            .field("stream-format", "adts")
            .build();
        src.set_caps(Some(&caps));
        src.set_stream_type(gstreamer_app::AppStreamType::Stream);
        src.set_format(gstreamer::Format::Time);
        src.set_max_bytes(1024 * 1024);
        src.set_block(true);
        Some(src)
    } else {
        None
    };

    // Embed metadata as MP4 tags via mp4mux's TagSetter interface
    if metadata.record_type.is_some()
        || metadata.start_time.is_some()
        || metadata.end_time.is_some()
        || metadata.camera_name.is_some()
    {
        let tag_setter = mp4mux
            .dynamic_cast_ref::<gstreamer::TagSetter>()
            .expect("mp4mux should implement TagSetter");
        // AI detection labels as keywords
        if let Some(ref rt) = metadata.record_type {
            let ai_tags: Vec<&str> = rt
                .split(',')
                .map(|s| s.trim())
                .filter(|s| {
                    matches!(
                        *s,
                        "people"
                            | "vehicle"
                            | "face"
                            | "dog_cat"
                            | "package"
                            | "visitor"
                            | "cry"
                            | "crossline"
                            | "intrusion"
                            | "loitering"
                            | "nonmotorveh"
                            | "md"
                            | "pir"
                            | "io"
                            | "other"
                            | "legacy"
                            | "loss"
                    )
                })
                .collect();
            if !ai_tags.is_empty() {
                let kw = ai_tags.join(", ");
                tag_setter.add_tag::<gstreamer::tags::Keywords>(
                    &kw.as_str(),
                    gstreamer::TagMergeMode::Replace,
                );
            }
            let comment = format!("recordType: {}", rt);
            tag_setter.add_tag::<gstreamer::tags::Comment>(
                &comment.as_str(),
                gstreamer::TagMergeMode::Replace,
            );
        }
        // Build description from timing + camera info
        let mut desc_parts = Vec::new();
        if let Some(ref name) = metadata.camera_name {
            desc_parts.push(format!("Camera: {}", name));
            tag_setter.add_tag::<gstreamer::tags::Title>(
                &name.as_str(),
                gstreamer::TagMergeMode::Replace,
            );
        }
        if let Some(ref st) = metadata.start_time {
            desc_parts.push(format!("Start: {}", st));
        }
        if let Some(ref et) = metadata.end_time {
            desc_parts.push(format!("End: {}", et));
        }
        if !desc_parts.is_empty() {
            let desc = desc_parts.join("; ");
            tag_setter.add_tag::<gstreamer::tags::Description>(
                &desc.as_str(),
                gstreamer::TagMergeMode::Replace,
            );
        }
        tag_setter.add_tag::<gstreamer::tags::Encoder>(
            &"neolink replay",
            gstreamer::TagMergeMode::Replace,
        );
        log::info!("Replay GStreamer: set metadata tags on mp4mux TagSetter");
    }

    pipeline.set_state(State::Playing)?;

    let result = std::thread::scope(|scope| -> Result<()> {
        let video = scope.spawn(|| -> Result<()> {
            let result = (|| -> Result<()> {
                let mut reader = super::output::FrameReader::open(directory)?;
                while let Some((data, pts_us)) = reader.next()? {
                    let mut buffer = gstreamer::Buffer::from_mut_slice(data);
                    buffer
                        .get_mut()
                        .unwrap()
                        .set_pts(ClockTime::from_useconds(pts_us));
                    video_src.push_buffer(buffer).context("Push video frame")?;
                }
                Ok(())
            })();
            let eos = video_src.end_of_stream().context("Video EOS");
            result.and(eos.map(|_| ()))
        });
        let audio = scope.spawn(|| -> Result<()> {
            let Some(audio) = &audio_src else {
                return Ok(());
            };
            let result = (|| -> Result<()> {
                use std::io::Read;
                let mut reader =
                    std::io::BufReader::new(std::fs::File::open(directory.join("audio"))?);
                let mut pts_ns = 0u64;
                loop {
                    let mut header = [0u8; 7];
                    if reader.read(&mut header[..1])? == 0 {
                        break;
                    }
                    reader
                        .read_exact(&mut header[1..])
                        .context("Incomplete ADTS header")?;
                    anyhow::ensure!(
                        header[0] == 0xff && header[1] & 0xf0 == 0xf0,
                        "Invalid ADTS syncword"
                    );
                    let length = (((header[3] & 3) as usize) << 11)
                        | ((header[4] as usize) << 3)
                        | (header[5] as usize >> 5);
                    anyhow::ensure!(length >= 7, "Invalid ADTS frame length");
                    let mut data = vec![0u8; length];
                    data[..7].copy_from_slice(&header);
                    reader
                        .read_exact(&mut data[7..])
                        .context("Incomplete ADTS frame")?;
                    let frequencies = [
                        96000u64, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000,
                        11025, 8000, 7350,
                    ];
                    let frequency = frequencies
                        .get(((header[2] & 0x3c) >> 2) as usize)
                        .context("Invalid ADTS sample rate")?;
                    let duration = ((header[6] & 3) as u64 + 1) * 1024 * 1_000_000_000 / frequency;
                    let mut buffer = gstreamer::Buffer::from_mut_slice(data);
                    let buffer_ref = buffer.get_mut().unwrap();
                    buffer_ref.set_pts(ClockTime::from_nseconds(pts_ns));
                    buffer_ref.set_duration(ClockTime::from_nseconds(duration));
                    audio.push_buffer(buffer).context("Push AAC frame")?;
                    pts_ns += duration;
                }
                Ok(())
            })();
            let eos = audio.end_of_stream().context("Audio EOS");
            result.and(eos.map(|_| ()))
        });
        // The bus is watched while producers run so pipeline errors unblock both queues.
        let bus = pipeline.bus().context("Pipeline bus")?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        let bus_result = loop {
            if std::time::Instant::now() >= deadline {
                break Err(anyhow::anyhow!("MP4 mux timed out"));
            }
            if let Some(msg) = bus.timed_pop(ClockTime::from_seconds(1)) {
                match msg.view() {
                    MessageView::Eos(..) => break Ok(()),
                    MessageView::Error(err) => {
                        break Err(anyhow::anyhow!("GStreamer error: {:?}", err))
                    }
                    _ => {}
                }
            }
        };
        let _ = pipeline.set_state(State::Null);
        let video_result = video
            .join()
            .map_err(|_| anyhow::anyhow!("Video feeder panicked"))?;
        let audio_result = audio
            .join()
            .map_err(|_| anyhow::anyhow!("Audio feeder panicked"))?;
        bus_result?;
        video_result?;
        audio_result?;
        Ok(())
    });
    let _ = pipeline.set_state(State::Null);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use neolink_core::bcmedia::model::{BcMedia, BcMediaAac, BcMediaIframe};

    fn generate(description: &str, mut consume: impl FnMut(Vec<u8>, u32)) {
        let pipeline = gstreamer::parse::launch(description)
            .unwrap()
            .downcast::<Pipeline>()
            .unwrap();
        let sink = pipeline
            .by_name("output")
            .unwrap()
            .downcast::<gstreamer_app::AppSink>()
            .unwrap();
        pipeline.set_state(State::Playing).unwrap();
        loop {
            let Some(sample) = sink.try_pull_sample(ClockTime::from_seconds(10)) else {
                let eos = sink.is_eos();
                pipeline.set_state(State::Null).unwrap();
                assert!(eos, "Media generator timed out");
                break;
            };
            let buffer = sample.buffer().unwrap();
            consume(
                buffer.map_readable().unwrap().as_slice().to_vec(),
                buffer.pts().unwrap().useconds() as u32,
            );
        }
    }

    #[test]
    fn muxes_generated_video_and_audio_from_disk() {
        gstreamer::init().unwrap();
        // Encoders are needed only to generate this test input, not to download recordings.
        if ["x264enc", "avenc_aac"]
            .iter()
            .any(|name| gstreamer::ElementFactory::find(name).is_none())
        {
            eprintln!("Skipping generated MP4 test: x264enc/avenc_aac is unavailable");
            return;
        }
        let mut writer = super::super::output::RecordingWriter::new(None).unwrap();
        generate("videotestsrc num-buffers=120 pattern=snow ! video/x-raw,width=320,height=240,framerate=10/1 ! x264enc tune=zerolatency key-int-max=10 byte-stream=true ! h264parse ! video/x-h264,stream-format=byte-stream,alignment=au ! appsink name=output", |data, microseconds| {
            writer.push(BcMedia::Iframe(BcMediaIframe { video_type: VideoType::H264, data, microseconds, time: None })).unwrap();
        });
        generate("audiotestsrc num-buffers=192 ! audio/x-raw,rate=16000,channels=1 ! avenc_aac ! aacparse ! audio/mpeg,mpegversion=4,stream-format=adts ! appsink name=output", |data, _| {
            writer.push(BcMedia::Aac(BcMediaAac { data })).unwrap();
        });
        let output = writer.directory().join("generated.mp4");
        mux_recording_to_mp4(
            writer.directory(),
            VideoType::H264,
            true,
            &output,
            &Mp4Metadata::default(),
        )
        .unwrap();
        let bytes = std::fs::read(&output).unwrap();
        assert!(bytes.windows(4).any(|bytes| bytes == b"ftyp"));
        assert!(bytes.windows(4).any(|bytes| bytes == b"avc1"));
        assert!(bytes.windows(4).any(|bytes| bytes == b"mp4a"));
        assert!(bytes.windows(4).any(|bytes| bytes == b"mdat"));
        // Demux both tracks and verify EOS, rather than accepting the existence of a file alone.
        let pipeline = gstreamer::parse::launch(&format!("filesrc location=\"{}\" ! qtdemux name=demux demux.video_0 ! queue ! h264parse ! fakesink demux.audio_0 ! queue ! aacparse ! fakesink", output.to_string_lossy().replace('\\', "/"))).unwrap().downcast::<Pipeline>().unwrap();
        pipeline.set_state(State::Playing).unwrap();
        let message = pipeline.bus().unwrap().timed_pop_filtered(
            ClockTime::from_seconds(15),
            &[gstreamer::MessageType::Eos, gstreamer::MessageType::Error],
        );
        pipeline.set_state(State::Null).unwrap();
        assert!(
            matches!(
                message.as_ref().map(|message| message.view()),
                Some(MessageView::Eos(..))
            ),
            "Demux did not finish cleanly: {:?}",
            message
        );
        writer.close();
    }
}
