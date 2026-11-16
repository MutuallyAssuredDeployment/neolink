//! SD card replay and download: file lists, playback, and XML time-range download (MSG 143/144).
//!
//! Desktop app (FUN_180177b80) sends CMD 0x17d with a 0x944-byte payload: 20-byte inner header
//! then 0x930 body (channel, 32 zeros, path 0x3ff). Use MSG_ID_REPLAY_DESKTOP when MSG 8 returns 400.
//!
//! Response codes (header offset 0x10): 200 = accept, 300 = end (by-name), 331 = end (by-time),
//! 400 = reject. App only writes binary when response_code matches stored expected (e.g. Android +0x1c).
//! 32-byte metadata skip: app does it only for msg_id 5 (not 0x17d); see docs/BCMEDIA_REPLAY_FORMAT.md.

use super::stream::StreamData;
use super::{BcCamera, Error, Result};
use std::convert::TryFrom;
use crate::bc::{
    model::*,
    xml::{
        xml_ver, BcPayloads, BcXml, DayRecord, DayRecordList, DayRecords, EventAlarmType,
        FileInfo, FileInfoList, FindAlarmVideo, ReplayDateTime, ReplaySeek,
    },
};
use crate::bcmedia::codex::BcMediaCodex;
use crate::bcmedia::model::BcMedia;
use bytes::BytesMut;
use tokio_util::codec::Decoder;
use log::debug;
use std::time::SystemTime;
use time::{Date, Month, PrimitiveDateTime, Time};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::channel;
use tokio::task;
use tokio_util::sync::CancellationToken;

/// Desktop replay payload size: 20-byte inner header + 0x930 body (channel, 32 zeros, path).
pub const DESKTOP_REPLAY_PAYLOAD_LEN: usize = 0x944;
const DESKTOP_INNER_HEADER_LEN: usize = 20;
const DESKTOP_PATH_MAX: usize = 0x3ff;

/// E1 (Extension 1.1) per-packet envelope: optional 32-byte prefix then Extension XML, then decrypted payload.
const E1_REPLAY_PREFIX_LEN: usize = 32;

/// Strip E1 replay envelope from a packet body.
/// Layouts: (1) 32 bytes + `<?xml ...></Extension>[\r]\n` + payload, or (2) `<?xml ...></Extension>...` + payload.
/// Also supports any closing after `</Extension>` (e.g. `</xml>`); uses first `00dc` as fallback for media start.
/// Special case: if packet ends with `</Extension>\n`, return empty (payload starts in next packet).
/// Returns the payload slice (decrypted media); if the body doesn't match, returns the whole slice.
fn strip_e1_replay_envelope(data: &[u8]) -> &[u8] {
    // Check if packet ends with </Extension>\n or </Extension>\r\n (XML spans entire packet, payload in next packet)
    if data.len() >= 14 && (data.ends_with(b"</Extension>\n") || data.ends_with(b"</Extension>\r\n")) {
        if data.starts_with(b"<?xml") || (data.len() > E1_REPLAY_PREFIX_LEN && data[E1_REPLAY_PREFIX_LEN..].starts_with(b"<?xml")) {
            return &[];
        }
    }
    // Try layout (1): 32-byte prefix then XML
    if data.len() > E1_REPLAY_PREFIX_LEN && data[E1_REPLAY_PREFIX_LEN..].starts_with(b"<?xml") {
        if let Some(payload_start) = crate::bc::e1::e1_xml_end_offset(data, E1_REPLAY_PREFIX_LEN) {
            if payload_start <= data.len() {
                return &data[payload_start..];
            }
        }
    }
    // Try layout (2): body starts with XML (no 32-byte prefix)
    if data.len() >= 12 && data.starts_with(b"<?xml") {
        if let Some(payload_start) = crate::bc::e1::e1_xml_end_offset(data, 0) {
            if payload_start <= data.len() {
                return &data[payload_start..];
            }
        }
    }
    data
}

/// Decode decrypted replay/download payloads; BcMedia frames can span BC messages.
struct ReplayDecoder {
    codec: BcMediaCodex,
    buffer: BytesMut,
    container: bool,
    decoded_media: bool,
}

impl ReplayDecoder {
    fn new(strict: bool) -> Self {
        Self {
            codec: BcMediaCodex::new(strict),
            buffer: BytesMut::new(),
            container: false,
            decoded_media: false,
        }
    }

    fn decode(&mut self, data: &[u8]) -> Result<Vec<BcMedia>> {
        let data = strip_e1_replay_envelope(data);
        // Positive MP4 detection only. Large I-frames are ordinary BcMedia.
        if !self.decoded_media && self.buffer.len() <= 32
            && data.len() >= 8 && &data[4..8] == b"ftyp"
        {
            self.container = true;
            self.buffer.clear();
        }
        if self.container {
            return Ok(vec![BcMedia::RawReplayChunk(data.to_vec())]);
        }
        self.buffer.extend_from_slice(data);
        let mut frames = Vec::new();
        while let Some(frame) = self.codec.decode(&mut self.buffer)? {
            self.decoded_media |= !matches!(&frame, BcMedia::InfoV1(_) | BcMedia::InfoV2(_));
            frames.push(frame);
        }
        Ok(frames)
    }
}

/// MSG 143 uses FileInfoList XML on the wire, rather than the native SDK's
/// BC_DOWNLOAD_BY_TIME_INFO struct. The client's output path is local only.
fn download_by_time_xml(
    channel_id: u8,
    stream_type: u32,
    start_time: ReplayDateTime,
    end_time: ReplayDateTime,
) -> Result<BcXml> {
    let stream = match stream_type {
        0 => "mainStream",
        1 => "subStream",
        2 => "externStream",
        _ => return Err(Error::InvalidArgument {
            argument: "stream_type".to_string(),
            value: stream_type.to_string(),
        }),
    };
    if replay_datetime_duration_secs(&start_time, &end_time).is_none() {
        return Err(Error::InvalidArgument {
            argument: "time_range".to_string(),
            value: "invalid start/end time or end before start".to_string(),
        });
    }
    Ok(BcXml {
        file_info_list: Some(FileInfoList {
            version: Some(xml_ver()),
            file_info: vec![FileInfo {
                channel_id: Some(channel_id),
                support_sub: Some(u8::from(stream_type == 1)),
                stream_type: Some(stream.to_string()),
                start_time: Some(start_time),
                end_time: Some(end_time),
                ..Default::default()
            }],
        }),
        ..Default::default()
    })
}

/// Build the 0x944-byte payload for desktop-style replay (CMD 0x17d).
/// Layout: 20-byte inner header (2, 0x82f, 8, 500 LE) then 0x930 body: channel (4), 32 zeros, path (1023), padding.
pub fn build_desktop_replay_payload(channel_id: u8, path: &str) -> Vec<u8> {
    let mut out = vec![0u8; DESKTOP_REPLAY_PAYLOAD_LEN];
    // Inner 20-byte header (from Ghidra FUN_180177b80 local_b08)
    out[0..8].copy_from_slice(&2u64.to_le_bytes());
    out[8..12].copy_from_slice(&0x82fu32.to_le_bytes());
    out[12..16].copy_from_slice(&8u32.to_le_bytes());
    out[16..20].copy_from_slice(&500u32.to_le_bytes());
    // 0x930 body: offset 0 = channel (u32 LE)
    out[DESKTOP_INNER_HEADER_LEN..DESKTOP_INNER_HEADER_LEN + 4]
        .copy_from_slice(&(channel_id as u32).to_le_bytes());
    // offset 4..36 = 32 zeros (already zeroed)
    // offset 36..36+1023 = path, null-padded
    let path_start = DESKTOP_INNER_HEADER_LEN + 4 + 32;
    let path_len = path.as_bytes().len().min(DESKTOP_PATH_MAX);
    out[path_start..path_start + path_len].copy_from_slice(&path.as_bytes()[..path_len]);
    out
}

/// BC_DOWNLOAD_BY_TIME_INFO size (0x480 = 1152 bytes). Layout from APK JNA + native path at +0x60.
pub const BC_DOWNLOAD_BY_TIME_INFO_SIZE: usize = 0x480;
const SAVE_PATH_MAX: usize = 1024; // cSaveFileName at +0x60, max 1024 bytes

/// Build the native SDK's 0x480-byte BC_DOWNLOAD_BY_TIME_INFO struct.
/// This struct is not the XML payload sent over the wire by MSG 143.
/// Layout: iChannel (4), cUID (32), logicChnBitmap (8), startTime (6×i32), endTime (6×i32),
/// padding (4), cSaveFileName (1024), fileSize/curSize/processed (8 each), streamType (4), padding (4).
pub fn build_download_by_time_payload(
    channel_id: u8,
    uid: &[u8],
    logic_chn_bitmap: u64,
    start_time: &ReplayDateTime,
    end_time: &ReplayDateTime,
    save_path: &str,
    stream_type: u32,
) -> Vec<u8> {
    let mut out = vec![0u8; BC_DOWNLOAD_BY_TIME_INFO_SIZE];
    let mut off = 0;
    out[off..off + 4].copy_from_slice(&(channel_id as u32).to_le_bytes());
    off += 4;
    let uid_len = uid.len().min(32);
    out[off..off + uid_len].copy_from_slice(&uid[..uid_len]);
    off += 32;
    out[off..off + 8].copy_from_slice(&logic_chn_bitmap.to_le_bytes());
    off += 8;
    out[off..off + 4].copy_from_slice(&(start_time.year as i32).to_le_bytes());
    out[off + 4..off + 8].copy_from_slice(&(start_time.month as i32).to_le_bytes());
    out[off + 8..off + 12].copy_from_slice(&(start_time.day as i32).to_le_bytes());
    out[off + 12..off + 16].copy_from_slice(&(start_time.hour as i32).to_le_bytes());
    out[off + 16..off + 20].copy_from_slice(&(start_time.minute as i32).to_le_bytes());
    out[off + 20..off + 24].copy_from_slice(&(start_time.second as i32).to_le_bytes());
    off += 24;
    out[off..off + 4].copy_from_slice(&(end_time.year as i32).to_le_bytes());
    out[off + 4..off + 8].copy_from_slice(&(end_time.month as i32).to_le_bytes());
    out[off + 8..off + 12].copy_from_slice(&(end_time.day as i32).to_le_bytes());
    out[off + 12..off + 16].copy_from_slice(&(end_time.hour as i32).to_le_bytes());
    out[off + 16..off + 20].copy_from_slice(&(end_time.minute as i32).to_le_bytes());
    out[off + 20..off + 24].copy_from_slice(&(end_time.second as i32).to_le_bytes());
    off += 24;
    off += 4; // +0x5c: 4 bytes padding (already zeroed)
    let path_len = save_path
        .char_indices()
        .take_while(|(index, ch)| *index + ch.len_utf8() <= SAVE_PATH_MAX - 1)
        .map(|(index, ch)| index + ch.len_utf8())
        .last()
        .unwrap_or(0);
    out[off..off + path_len].copy_from_slice(&save_path.as_bytes()[..path_len]);
    // +0x478: streamType (4 bytes)
    out[0x478..0x47c].copy_from_slice(&stream_type.to_le_bytes());
    out
}

/// Duration in seconds between two ReplayDateTimes (end − start). Returns None if invalid or negative.
pub fn replay_datetime_duration_secs(start: &ReplayDateTime, end: &ReplayDateTime) -> Option<u64> {
    let sm = Month::try_from(start.month).ok()?;
    let em = Month::try_from(end.month).ok()?;
    let sd = Date::from_calendar_date(start.year, sm, start.day).ok()?;
    let ed = Date::from_calendar_date(end.year, em, end.day).ok()?;
    let st = Time::from_hms(start.hour, start.minute, start.second).ok()?;
    let et = Time::from_hms(end.hour, end.minute, end.second).ok()?;
    let start_dt = PrimitiveDateTime::new(sd, st).assume_utc();
    let end_dt = PrimitiveDateTime::new(ed, et).assume_utc();
    let secs = (end_dt - start_dt).whole_seconds();
    if secs < 0 {
        None
    } else {
        Some(secs as u64)
    }
}

/// Parse ReplayDateTime from a filename like `01_20260204120000` (channel_YYYYMMDDHHmmss).
fn parse_seek_time_from_name(name: &str) -> Option<ReplayDateTime> {
    let digits: Vec<u8> = name
        .bytes()
        .filter(|b| b.is_ascii_digit())
        .map(|b| b - b'0')
        .collect();
    if digits.len() < 14 {
        return None;
    }
    let d = &digits[digits.len() - 14..];
    let year = (d[0] as i32) * 1000 + (d[1] as i32) * 100 + (d[2] as i32) * 10 + (d[3] as i32);
    let month = d[4] * 10 + d[5];
    let day = d[6] * 10 + d[7];
    let hour = d[8] * 10 + d[9];
    let minute = d[10] * 10 + d[11];
    let second = d[12] * 10 + d[13];
    if month == 0 || month > 12 || day == 0 || day > 31 || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    Some(ReplayDateTime {
        year,
        month,
        day,
        hour,
        minute,
        second,
    })
}

impl BcCamera {
    /// Get day records in a time range (MSG 142). Returns which days have recordings.
    pub async fn get_day_records(
        &self,
        start_time: ReplayDateTime,
        end_time: ReplayDateTime,
    ) -> Result<DayRecords> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection
            .subscribe(MSG_ID_DAY_RECORDS, msg_num)
            .await?;

        let day_records = DayRecords {
            version: xml_ver(),
            start_time: start_time.clone(),
            end_time: end_time.clone(),
            day_record_list: DayRecordList {
                day_record: vec![DayRecord {
                    index: 0,
                    channel_id: self.channel_id,
                }],
            },
            day_type_list: None,
        };

        let msg = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_DAY_RECORDS,
                channel_id: self.channel_id,
                msg_num,
                stream_type: 0,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::ModernMsg(ModernMsg {
                extension: None,
                payload: Some(BcPayloads::BcXml(BcXml {
                    day_records: Some(day_records),
                    ..Default::default()
                })),
            }),
        };

        sub.send(msg).await?;
        let msg = sub.recv().await?;

        if msg.meta.response_code != 200 {
            return Err(Error::CameraServiceUnavailable {
                id: msg.meta.msg_id,
                code: msg.meta.response_code,
            });
        }

        if let BcBody::ModernMsg(ModernMsg {
            payload:
                Some(BcPayloads::BcXml(BcXml {
                    day_records: Some(ref data),
                    ..
                })),
            ..
        }) = msg.body
        {
            debug!(
                "DayRecords response: day_type_list present = {}, entries = {}; full response: {:?}",
                data.day_type_list.is_some(),
                data.day_type_list
                    .as_ref()
                    .map(|l| l.day_type.len())
                    .unwrap_or(0),
                data
            );
            Ok(data.clone())
        } else {
            Err(Error::UnintelligibleReply {
                _reply: std::sync::Arc::new(Box::new(msg)),
                why: "Expected DayRecords xml in response",
            })
        }
    }

    /// Get file list handle for a day (MSG 14). Returns a handle to use with get_file_list_by_handle.
    pub async fn get_file_list_handle(
        &self,
        stream_type: &str,
        record_type: &str,
        start_time: ReplayDateTime,
        end_time: ReplayDateTime,
    ) -> Result<FileInfo> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection
            .subscribe(MSG_ID_REPLAY_FILE_LIST_HANDLE, msg_num)
            .await?;

        let file_info = FileInfo {
            channel_id: Some(self.channel_id),
            record_type: Some(record_type.to_string()),
            support_sub: Some(0),
            start_time: Some(start_time.clone()),
            end_time: Some(end_time.clone()),
            stream_type: Some(stream_type.to_string()),
            ..Default::default()
        };

        let xml = BcXml {
            file_info_list: Some(FileInfoList {
                version: Some(xml_ver()),
                file_info: vec![file_info],
            }),
            ..Default::default()
        };

        let msg = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_REPLAY_FILE_LIST_HANDLE,
                channel_id: self.channel_id,
                msg_num,
                stream_type: 0,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::ModernMsg(ModernMsg {
                extension: None,
                payload: Some(BcPayloads::BcXml(xml)),
            }),
        };

        sub.send(msg).await?;
        let msg = sub.recv().await?;

        if msg.meta.response_code != 200 {
            return Err(Error::CameraServiceUnavailable {
                id: msg.meta.msg_id,
                code: msg.meta.response_code,
            });
        }

        if let BcBody::ModernMsg(ModernMsg {
            payload:
                Some(BcPayloads::BcXml(BcXml {
                    file_info_list: Some(ref list),
                    ..
                })),
            ..
        }) = msg.body
        {
            if list.file_info.is_empty() {
                return Err(Error::UnintelligibleReply {
                    _reply: std::sync::Arc::new(Box::new(msg)),
                    why: "FileInfoList response had no FileInfo",
                });
            }
            Ok(list.file_info[0].clone())
        } else {
            Err(Error::UnintelligibleReply {
                _reply: std::sync::Arc::new(Box::new(msg)),
                why: "Expected FileInfoList xml in response",
            })
        }
    }

    /// List files by handle from get_file_list_handle (MSG 15).
    pub async fn get_file_list_by_handle(&self, handle: u32) -> Result<Vec<FileInfo>> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection
            .subscribe(MSG_ID_REPLAY_FILE_LIST, msg_num)
            .await?;

        let file_info = FileInfo {
            channel_id: Some(self.channel_id),
            handle: Some(handle),
            ..Default::default()
        };

        let xml = BcXml {
            file_info_list: Some(FileInfoList {
                version: Some(xml_ver()),
                file_info: vec![file_info],
            }),
            ..Default::default()
        };

        let msg = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_REPLAY_FILE_LIST,
                channel_id: self.channel_id,
                msg_num,
                stream_type: 0,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::ModernMsg(ModernMsg {
                extension: None,
                payload: Some(BcPayloads::BcXml(xml)),
            }),
        };

        sub.send(msg).await?;
        let msg = sub.recv().await?;

        if msg.meta.response_code != 200 {
            return Err(Error::CameraServiceUnavailable {
                id: msg.meta.msg_id,
                code: msg.meta.response_code,
            });
        }

        if let BcBody::ModernMsg(ModernMsg {
            payload:
                Some(BcPayloads::BcXml(BcXml {
                    file_info_list: Some(ref list),
                    ..
                })),
            ..
        }) = msg.body
        {
            Ok(list.file_info.clone())
        } else {
            Err(Error::UnintelligibleReply {
                _reply: std::sync::Arc::new(Box::new(msg)),
                why: "Expected FileInfoList xml in response",
            })
        }
    }

    /// Alarm video search: START (MSG 175). Returns the response containing a fileHandle.
    /// `alarm_types` is a list of alarm/AI tags (e.g. ["md", "people", "vehicle"]).
    /// `stream_type_num` is 0 for mainStream, 1 for subStream.
    pub async fn alarm_video_search_start(
        &self,
        stream_type_num: u8,
        alarm_types: &[&str],
        start_time: ReplayDateTime,
        end_time: ReplayDateTime,
    ) -> Result<FindAlarmVideo> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection
            .subscribe(MSG_ID_ALARM_VIDEO_SEARCH, msg_num)
            .await?;

        let alarm_type_str = alarm_types.join(", ");
        let xml = BcXml {
            find_alarm_video: Some(FindAlarmVideo {
                version: Some(xml_ver()),
                channel_id: Some(self.channel_id),
                file_handle: None,
                stream_type: Some(stream_type_num),
                not_search_video: Some(0),
                start_time: Some(start_time),
                end_time: Some(end_time),
                alarm_type: Some(alarm_type_str),
                event_alarm_type: Some(EventAlarmType {
                    items: alarm_types.iter().map(|s| s.to_string()).collect(),
                }),
            }),
            ..Default::default()
        };

        let msg = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_ALARM_VIDEO_SEARCH,
                channel_id: self.channel_id,
                msg_num,
                stream_type: 0,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::ModernMsg(ModernMsg {
                extension: None,
                payload: Some(BcPayloads::BcXml(xml)),
            }),
        };

        sub.send(msg).await?;
        let msg = sub.recv().await?;

        if msg.meta.response_code != 200 {
            return Err(Error::CameraServiceUnavailable {
                id: msg.meta.msg_id,
                code: msg.meta.response_code,
            });
        }

        if let BcBody::ModernMsg(ModernMsg {
            payload:
                Some(BcPayloads::BcXml(BcXml {
                    find_alarm_video: Some(ref fav),
                    ..
                })),
            ..
        }) = msg.body
        {
            Ok(fav.clone())
        } else {
            Err(Error::UnintelligibleReply {
                _reply: std::sync::Arc::new(Box::new(msg)),
                why: "Expected findAlarmVideo xml in response",
            })
        }
    }

    /// Alarm video search: DO/paginate (MSG 175). Send a fileHandle to get the next batch of events.
    /// Returns the AlarmEventList from the response.
    pub async fn alarm_video_search_next(
        &self,
        file_handle: i32,
    ) -> Result<FindAlarmVideo> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection
            .subscribe(MSG_ID_ALARM_VIDEO_SEARCH, msg_num)
            .await?;

        let xml = BcXml {
            find_alarm_video: Some(FindAlarmVideo {
                version: Some(xml_ver()),
                file_handle: Some(file_handle),
                ..Default::default()
            }),
            ..Default::default()
        };

        let msg = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_ALARM_VIDEO_SEARCH,
                channel_id: self.channel_id,
                msg_num,
                stream_type: 0,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::ModernMsg(ModernMsg {
                extension: None,
                payload: Some(BcPayloads::BcXml(xml)),
            }),
        };

        sub.send(msg).await?;
        let msg = sub.recv().await?;

        if msg.meta.response_code != 200 {
            return Err(Error::CameraServiceUnavailable {
                id: msg.meta.msg_id,
                code: msg.meta.response_code,
            });
        }

        if let BcBody::ModernMsg(ModernMsg {
            payload:
                Some(BcPayloads::BcXml(BcXml {
                    find_alarm_video: Some(ref fav),
                    ..
                })),
            ..
        }) = msg.body
        {
            Ok(fav.clone())
        } else {
            Err(Error::UnintelligibleReply {
                _reply: std::sync::Arc::new(Box::new(msg)),
                why: "Expected findAlarmVideo xml in response",
            })
        }
    }

    /// Get file details by name (MSG 13). Use name from get_file_list_by_handle.
    /// App (FUN_1801740c0) sends supportSub=1 for subStream and only includes playSpeed when in [1,32].
    pub async fn get_file_by_name(
        &self,
        name: &str,
        support_sub: u8,
        play_speed: u32,
        stream_type: &str,
    ) -> Result<FileInfo> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection
            .subscribe(MSG_ID_REPLAY_FILE_BY_NAME, msg_num)
            .await?;

        let support_sub = if stream_type == "subStream" { 1 } else { support_sub };
        let file_info = FileInfo {
            channel_id: Some(self.channel_id),
            name: Some(name.to_string()),
            support_sub: Some(support_sub),
            play_speed: (1..=32).contains(&play_speed).then_some(play_speed),
            stream_type: Some(stream_type.to_string()),
            ..Default::default()
        };

        let xml = BcXml {
            file_info_list: Some(FileInfoList {
                version: Some(xml_ver()),
                file_info: vec![file_info],
            }),
            ..Default::default()
        };

        let msg = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_REPLAY_FILE_BY_NAME,
                channel_id: self.channel_id,
                msg_num,
                stream_type: 0,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::ModernMsg(ModernMsg {
                extension: None,
                payload: Some(BcPayloads::BcXml(xml)),
            }),
        };

        sub.send(msg).await?;
        let msg = sub.recv().await?;

        if msg.meta.response_code != 200 {
            return Err(Error::CameraServiceUnavailable {
                id: msg.meta.msg_id,
                code: msg.meta.response_code,
            });
        }

        if let BcBody::ModernMsg(ModernMsg {
            payload:
                Some(BcPayloads::BcXml(BcXml {
                    file_info_list: Some(ref list),
                    ..
                })),
            ..
        }) = msg.body
        {
            if list.file_info.is_empty() {
                return Err(Error::UnintelligibleReply {
                    _reply: std::sync::Arc::new(Box::new(msg)),
                    why: "FileInfoList response had no FileInfo",
                });
            }
            Ok(list.file_info[0].clone())
        } else {
            Err(Error::UnintelligibleReply {
                _reply: std::sync::Arc::new(Box::new(msg)),
                why: "Expected FileInfoList xml in response",
            })
        }
    }

    /// Get file metadata (duration, size) from the file list for a given replay file name.
    /// Fetches the file list for the file's day and finds the matching entry.
    /// Returns None if the name cannot be parsed (no date) or the file is not in the list.
    pub async fn get_replay_file_metadata(
        &self,
        name: &str,
        stream_type: &str,
        record_type: &str,
    ) -> Result<Option<FileInfo>> {
        let seek_time = match parse_seek_time_from_name(name) {
            Some(t) => t,
            None => return Ok(None),
        };
        let day_start = ReplayDateTime {
            hour: 0,
            minute: 0,
            second: 0,
            ..seek_time.clone()
        };
        let day_end = ReplayDateTime {
            hour: 23,
            minute: 59,
            second: 59,
            ..seek_time
        };
        let handle_info = self
            .get_file_list_handle(stream_type, record_type, day_start, day_end)
            .await?;
        let handle = match handle_info.handle {
            Some(h) => h,
            None => return Ok(None),
        };
        let files = self.get_file_list_by_handle(handle).await?;
        let found = files
            .into_iter()
            .find(|f| f.name.as_deref() == Some(name));
        Ok(found)
    }

    /// Get replay file duration in seconds from the file list (start_time..end_time).
    /// Returns None if metadata cannot be obtained or start/end time are missing.
    pub async fn get_replay_file_duration_secs(
        &self,
        name: &str,
        stream_type: &str,
        record_type: &str,
    ) -> Result<Option<u64>> {
        let meta = self
            .get_replay_file_metadata(name, stream_type, record_type)
            .await?;
        let duration = meta.and_then(|info| {
            let start = info.start_time.as_ref()?;
            let end = info.end_time.as_ref()?;
            replay_datetime_duration_secs(start, end)
        });
        Ok(duration)
    }

    /// Replay seek: prepare playback position (MSG 123). seq is epoch seconds; seek_time from file list.
    pub async fn replay_seek(
        &self,
        seq: u32,
        seek_time: ReplayDateTime,
    ) -> Result<()> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection
            .subscribe(MSG_ID_REPLAY_SEEK, msg_num)
            .await?;

        let replay_seek = ReplaySeek {
            version: xml_ver(),
            channel_id: self.channel_id,
            seq,
            seek_time,
        };

        let msg = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_REPLAY_SEEK,
                channel_id: self.channel_id,
                msg_num,
                stream_type: 0,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::ModernMsg(ModernMsg {
                extension: None,
                payload: Some(BcPayloads::BcXml(BcXml {
                    replay_seek: Some(replay_seek),
                    ..Default::default()
                })),
            }),
        };

        sub.send(msg).await?;
        let msg = sub.recv().await?;

        if msg.meta.response_code == 200 {
            Ok(())
        } else {
            Err(Error::CameraServiceUnavailable {
                id: msg.meta.msg_id,
                code: msg.meta.response_code,
            })
        }
    }

    /// Start replay playback (MSG 5). The caller sends replay_stop (MSG 7)
    /// and shuts down the returned stream when finished.
    ///
    /// Caller should have obtained `name` from `get_file_list_by_handle`. The protocol requires
    /// ReplaySeek (MSG 123) before get_file_by_name (MSG 13); we parse seek time from the
    /// filename (e.g. `01_20260204120000` → 2026-02-04 12:00:00) when possible.
    ///
    /// File-list sizes describe the stored file, which can differ from BcMedia
    /// transfer bytes. Only an end response or inactivity completes replay.
    pub async fn start_replay(
        &self,
        name: &str,
        stream_type: &str,
        play_speed: u32,
        strict: bool,
        buffer_size: usize,
        dump_replay: Option<std::path::PathBuf>,
        dump_replay_limit: Option<usize>,
        _expected_size: Option<u64>,
    ) -> Result<StreamData> {
        let seq = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs() as u32)
            .unwrap_or(0);

        // Protocol order: ReplaySeek (123) before get_file_by_name (13). Use seek time from
        // filename (e.g. 01_20260204120000 → 2026-02-04 12:00:00) or start of today.
        let seek_time = parse_seek_time_from_name(name).unwrap_or_else(|| {
            let now = time::OffsetDateTime::now_utc();
            ReplayDateTime {
                year: now.year(),
                month: now.month() as u8,
                day: now.day() as u8,
                hour: 0,
                minute: 0,
                second: 0,
            }
        });
        self.replay_seek(seq, seek_time.clone()).await?;
        log::info!("Replay: seek (MSG 123) done");

        // Read camera-specific file metadata when available. Some models reject MSG 13,
        // so fall back to the timestamp in the filename and UID 0 for those cameras.
        let filename_start_time = parse_seek_time_from_name(name);
        let file_info = match self
            .get_file_by_name(name, 0, play_speed, stream_type)
            .await
        {
            Ok(info) => Some(info),
            Err(
                e @ Error::CameraServiceUnavailable {
                    id: MSG_ID_REPLAY_FILE_BY_NAME,
                    code: 400 | 405,
                },
            ) if filename_start_time.is_some() => {
                log::debug!(
                    "Replay: MSG 13 metadata unavailable; using filename metadata: {}",
                    e
                );
                None
            }
            Err(e) => return Err(e),
        };
        let replay_uid = file_info.as_ref().and_then(|info| info.uid).unwrap_or(0);
        let start_time = file_info
            .and_then(|info| info.start_time)
            .or(filename_start_time)
            .ok_or_else(|| {
                Error::UnintelligibleReply {
                    _reply: std::sync::Arc::new(Box::new(Bc::new_from_meta(BcMeta {
                        msg_id: 0,
                        channel_id: 0,
                        stream_type: 0,
                        response_code: 0,
                        msg_num: 0,
                        class: 0,
                    }))),
                    why: "FileInfo from camera had no startTime",
                }
            })?;

        log::info!("Replay: start_time {:04}-{:02}-{:02} {:02}:{:02}:{:02}", start_time.year, start_time.month, start_time.day, start_time.hour, start_time.minute, start_time.second);
        let connection = self.get_connection();
        let start_msg_num = self.new_message_num();
        let channel_id = self.channel_id;
        let name = name.to_string();
        let stream_type = stream_type.to_string();
        let dump_path = dump_replay;

        let buffer_size = if buffer_size == 0 { 100 } else { buffer_size };
        const DEFAULT_DUMP_LIMIT: usize = 131072;
        let dump_limit = dump_replay_limit.unwrap_or(DEFAULT_DUMP_LIMIT);
        let (tx, rx) = channel(buffer_size);
        let abort_handle = CancellationToken::new();
        let abort_handle_thread = abort_handle.clone();

        let handle = task::spawn(async move {
            // Optional dump of decrypted BcMedia/container data. Limit 0 = full stream.
            let mut dump_file = None::<(tokio::fs::File, Option<usize>)>;
            if let Some(ref p) = dump_path {
                match tokio::fs::File::create(p).await {
                    Ok(f) => {
                        let limit_str = if dump_limit == 0 {
                            "full stream".to_string()
                        } else {
                            format!("first {} bytes", dump_limit)
                        };
                        log::info!(
                            "Replay: will dump {} of decrypted replay data to {}",
                            limit_str,
                            p.display()
                        );
                        dump_file = Some((f, if dump_limit == 0 { None } else { Some(dump_limit) }));
                    }
                    Err(e) => log::warn!("Replay: could not create dump file {}: {}", p.display(), e),
                }
            }

            // Try MSG 5 first (replay file download as in app pcap: 123 + 5). Some cameras (e.g. E1)
            // return 400 for MSG 5; then try MSG 8 (same FileInfoList body). If MSG 8 returns 400, try desktop (0x17d).
            // Body: FileInfoList version="1.1" → FileInfo with uid, name, channelId, supportSub, streamType, startTime.
            let mut msg_id = MSG_ID_REPLAY_START; // 5 = try first (pcap flow)
            let mut sub = connection
                .subscribe(msg_id, start_msg_num)
                .await?;

            let support_sub = if stream_type.as_str() == "subStream" { 1 } else { 0 };
            let file_info = FileInfo {
                uid: Some(replay_uid),
                id: None,
                name: Some(name.clone()),
                channel_id: Some(channel_id),
                support_sub: Some(support_sub),
                stream_type: Some(stream_type.clone()),
                start_time: Some(start_time),
                ..Default::default()
            };
            let xml = BcXml {
                file_info_list: Some(FileInfoList {
                    version: Some(xml_ver()),
                    file_info: vec![file_info.clone()],
                }),
                ..Default::default()
            };
            let msg = Bc {
                meta: BcMeta {
                    msg_id,
                    channel_id,
                    msg_num: start_msg_num,
                    stream_type: 0,
                    response_code: 0,
                    class: 0x6414,
                },
                body: BcBody::ModernMsg(ModernMsg {
                    extension: None,
                    payload: Some(BcPayloads::BcXml(xml)),
                }),
            };
            log::info!(
                "Replay: sending MSG {} start_replay name={} channel={} streamType={}",
                msg_id,
                name,
                channel_id,
                stream_type
            );
            sub.send(msg).await?;
            log::info!("Replay: MSG {} sent, waiting for first response...", msg_id);

            let mut first = true;
            let mut decoder = ReplayDecoder::new(strict);
            let mut packet_count = 0u32;
            let mut total_binary_bytes = 0usize;
            const RECV_TIMEOUT_SECS: u64 = 15;
            let recv_timeout = tokio::time::Duration::from_secs(RECV_TIMEOUT_SECS);
            'recv_loop: loop {
                tokio::select! {
                    _ = abort_handle_thread.cancelled() => break 'recv_loop,
                    msg_res = tokio::time::timeout(recv_timeout, sub.recv()) => {
                        let msg_res = match msg_res {
                            Err(_) => {
                                log::info!(
                                    "Replay: no data for {}s (camera may have stopped without 300), finishing ({} packets, {} bytes)",
                                    RECV_TIMEOUT_SECS, packet_count, total_binary_bytes
                                );
                                tokio::select! {
                                    _ = abort_handle_thread.cancelled() => {},
                                    _ = tx.send(Ok(BcMedia::StreamEnd)) => {},
                                }
                                break 'recv_loop;
                            }
                            Ok(r) => r,
                        };
                        let msg = match msg_res {
                            Err(e) => {
                                log::info!(
                                    "Replay stream ended: recv error ({} packets, {} bytes binary): {:?}",
                                    packet_count, total_binary_bytes, e
                                );
                                log::info!(
                                    "Replay failed: verify protocol in Ghidra via MCP (see GHIDRA_CHECKLIST.md), do not guess"
                                );
                                return Err(e);
                            }
                            Ok(m) => m,
                        };
                        let body_type = match &msg.body {
                            BcBody::ModernMsg(ModernMsg { payload: Some(BcPayloads::Binary(d)), .. }) => {
                                format!("binary({} bytes)", d.len())
                            }
                            BcBody::ModernMsg(ModernMsg { payload: Some(BcPayloads::BcXml(_)), .. }) => "xml".to_string(),
                            _ => "other".to_string(),
                        };
                        log::debug!(
                            "Replay: recv msg_id={} msg_num={} response_code={} body={}",
                            msg.meta.msg_id,
                            msg.meta.msg_num,
                            msg.meta.response_code,
                            body_type
                        );
                        if first {
                            first = false;
                            if matches!(msg.meta.response_code, 400 | 405)
                                && msg_id == MSG_ID_REPLAY_START
                            {
                                // Camera rejected MSG 5; try MSG 8 (e.g. E1).
                                log::info!(
                                    "Replay: camera returned 400 for MSG 5, trying MSG 8..."
                                );
                                drop(sub);
                                msg_id = MSG_ID_REPLAY_START_ALT;
                                sub = connection.subscribe(msg_id, start_msg_num).await?;
                                let xml8 = BcXml {
                                    file_info_list: Some(FileInfoList {
                                        version: Some(xml_ver()),
                                        file_info: vec![file_info.clone()],
                                    }),
                                    ..Default::default()
                                };
                                let msg8 = Bc {
                                    meta: BcMeta {
                                        msg_id,
                                        channel_id,
                                        msg_num: start_msg_num,
                                        stream_type: 0,
                                        response_code: 0,
                                        class: 0x6414,
                                    },
                                    body: BcBody::ModernMsg(ModernMsg {
                                        extension: None,
                                        payload: Some(BcPayloads::BcXml(xml8)),
                                    }),
                                };
                                sub.send(msg8).await?;
                                log::info!("Replay: MSG 8 sent, waiting for first response...");
                                first = true;
                                continue 'recv_loop;
                            }
                            if matches!(msg.meta.response_code, 400 | 405)
                                && msg_id == MSG_ID_REPLAY_START_ALT
                            {
                                // Camera rejected MSG 8; try desktop binary replay (CMD 0x17d, 0x944 payload).
                                log::info!(
                                    "Replay: camera returned 400 for MSG 8, trying desktop replay (0x17d)..."
                                );
                                drop(sub);
                                msg_id = MSG_ID_REPLAY_DESKTOP;
                                let sub_desktop = connection
                                    .subscribe(MSG_ID_REPLAY_DESKTOP, start_msg_num)
                                    .await?;
                                let payload = build_desktop_replay_payload(channel_id, &name);
                                let desktop_msg = Bc {
                                    meta: BcMeta {
                                        msg_id: MSG_ID_REPLAY_DESKTOP,
                                        channel_id,
                                        msg_num: start_msg_num,
                                        stream_type: 0,
                                        response_code: 0,
                                        class: 0x6414,
                                    },
                                    body: BcBody::ModernMsg(ModernMsg {
                                        extension: None,
                                        payload: Some(BcPayloads::Binary(payload)),
                                    }),
                                };
                                sub_desktop.send(desktop_msg).await?;
                                log::info!("Replay: desktop 0x17d sent, waiting for response...");
                                sub = sub_desktop;
                                first = true;
                                continue 'recv_loop;
                            }
                            if msg.meta.response_code != 200 {
                                log::info!(
                                    "Replay start rejected by camera: response_code={}",
                                    msg.meta.response_code
                                );
                                return Err(Error::UnintelligibleReply {
                                    _reply: std::sync::Arc::new(Box::new(msg)),
                                    why: "Camera did not accept replay start",
                                });
                            }
                            log::info!("Replay: camera accepted (200), streaming...");
                            // Tell the consumer which replay request was accepted.
                            let _ = tx.send(Ok(BcMedia::ReplayStarted(msg_id as u16))).await;
                        } else if msg.meta.response_code == 300 || msg.meta.response_code == 331 {
                            // Camera signals end of file (300 = by-name; 331 = by-time). Notify consumer then stop.
                            log::info!(
                                "Replay/Download: response {} (end of file), stopping ({} packets, {} bytes)",
                                msg.meta.response_code, packet_count, total_binary_bytes
                            );
                            tokio::select! {
                                _ = abort_handle_thread.cancelled() => {},
                                _ = tx.send(Ok(BcMedia::StreamEnd)) => {},
                            }
                            break 'recv_loop;
                        }
                        if let BcBody::ModernMsg(ModernMsg {
                            payload: Some(BcPayloads::Binary(ref data)),
                            ..
                        }) = msg.body
                        {
                            packet_count += 1;
                            total_binary_bytes += data.len();
                            if let Some((ref mut f, ref mut remaining)) = dump_file {
                                let to_write = remaining.map_or(data.len(), |r| data.len().min(r));
                                if to_write > 0 {
                                    f.write_all(&data[..to_write]).await?;
                                    if let Some(r) = remaining {
                                        *r -= to_write;
                                    }
                                }
                            }
                            for frame in decoder.decode(data)? {
                                if tokio::select! {
                                    _ = abort_handle_thread.cancelled() => true,
                                    sent = tx.send(Ok(frame)) => sent.is_err(),
                                } {
                                    break 'recv_loop;
                                }
                            }
                            if packet_count <= 3 || packet_count % 200 == 0 {
                                log::info!(
                                    "Replay: {} packets, {} KB received",
                                    packet_count, total_binary_bytes / 1024
                                );
                            }
                        } else {
                            log::info!(
                                "Replay: message has no binary payload (response_code={})",
                                msg.meta.response_code
                            );
                        }
                    }
                }
            }
            // Flush dump file so any written bytes are persisted (e.g. when stream ends before 128KB).
            if let Some((ref mut f, _)) = dump_file {
                let _ = f.flush().await;
            }
            log::info!(
                "Replay stream ended: {} packets, {} bytes binary",
                packet_count, total_binary_bytes
            );

            // Do not send MSG 7 (stop) here. The caller (replay play command) sends it via
            // replay_stop() after receiving StreamEnd. Sending it here would compete for the same
            // connection and can deadlock: main holds the connection in replay_stop (waiting for
            // response) while this task blocks on subscribe(), so this task never exits and the
            // process hangs when joining the stream handle.

            Ok(())
        });

        Ok(StreamData::from_parts(handle, rx, abort_handle))
    }

    /// Download BcMedia by time using MSG 143 FileInfoList XML. Ends on response
    /// 300 or 331; MSG 144 is sent on completion, cancellation, or receive failure.
    pub async fn start_download_by_time(
        &self,
        start_time: ReplayDateTime,
        end_time: ReplayDateTime,
        _save_path: &str,
        stream_type: u32,
        strict: bool,
        buffer_size: usize,
        dump_replay: Option<std::path::PathBuf>,
        dump_replay_limit: Option<usize>,
    ) -> Result<StreamData> {
        let connection = self.get_connection();
        let start_msg_num = self.new_message_num();
        let stop_msg_num = self.new_message_num();
        let channel_id = self.channel_id;
        let xml = download_by_time_xml(channel_id, stream_type, start_time, end_time)?;
        let start_msg = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_DOWNLOAD_BY_TIME,
                channel_id,
                msg_num: start_msg_num,
                stream_type: 0,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::ModernMsg(ModernMsg {
                extension: None,
                payload: Some(BcPayloads::BcXml(xml)),
            }),
        };
        let (tx, rx) = channel(if buffer_size == 0 { 100 } else { buffer_size });
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<()>>();
        let abort_handle = CancellationToken::new();
        let abort_handle_thread = abort_handle.clone();
        let handle = task::spawn(async move {
            // The task owns the connection; create its borrowing subscription here.
            // Keep the same subscription through startup and media reception so no
            // packets are lost between acceptance and returning the stream.
            let started: Result<_> = async {
                let mut sub = connection.subscribe(MSG_ID_DOWNLOAD_BY_TIME, start_msg_num).await?;
                sub.send(start_msg).await?;
                let first_msg = tokio::time::timeout(
                    std::time::Duration::from_secs(15), sub.recv()
                ).await??;
                if first_msg.meta.response_code != 200 {
                    return Err(Error::CameraServiceUnavailable {
                        id: MSG_ID_DOWNLOAD_BY_TIME,
                        code: first_msg.meta.response_code,
                    });
                }
                Ok((sub, first_msg))
            }.await;
            let (mut sub, first_msg) = match started {
                Ok(started) => {
                    log::info!("DownloadByTime: camera accepted MSG 143 XML, receiving BcMedia");
                    if ready_tx.send(Ok(())).is_err() {
                        // The caller disappeared during startup. Stop the accepted
                        // download through the normal cancellation/cleanup path.
                        abort_handle_thread.cancel();
                    }
                    started
                }
                Err(err) => {
                    let _ = ready_tx.send(Err(err.clone()));
                    return Err(err);
                }
            };
            let result: Result<()> = async {
                let mut dump_file = match dump_replay {
                    Some(path) => Some(tokio::fs::File::create(path).await?),
                    None => None,
                };
                let mut dump_remaining = match dump_replay_limit.unwrap_or(131072) {
                    0 => None,
                    n => Some(n),
                };
                let mut decoder = ReplayDecoder::new(strict);
                let mut pending = Some(first_msg);
                let mut packets = 0u32;
                let mut received_bytes = 0usize;
                'recv_loop: loop {
                    let msg = match pending.take() {
                        Some(msg) => msg,
                        None => tokio::select! {
                            _ = abort_handle_thread.cancelled() => break 'recv_loop,
                            msg = tokio::time::timeout(
                                std::time::Duration::from_secs(15), sub.recv()
                            ) => msg??,
                        },
                    };
                    if matches!(msg.meta.response_code, 300 | 331) {
                        log::info!(
                            "DownloadByTime: end response {}, {} packets, {} BcMedia bytes",
                            msg.meta.response_code, packets, received_bytes
                        );
                        if !decoder.buffer.is_empty() {
                            return Err(Error::UnintelligibleReply {
                                _reply: std::sync::Arc::new(Box::new(msg)),
                                why: "Download ended with an incomplete BcMedia frame",
                            });
                        }
                        tokio::select! {
                            _ = abort_handle_thread.cancelled() => {},
                            _ = tx.send(Ok(BcMedia::StreamEnd)) => {},
                        }
                        break 'recv_loop;
                    }
                    if let BcBody::ModernMsg(ModernMsg {
                        payload: Some(BcPayloads::Binary(ref data)), ..
                    }) = msg.body {
                        packets += 1;
                        received_bytes += data.len();
                        if let Some(ref mut file) = dump_file {
                            let n = dump_remaining.map_or(data.len(), |r| data.len().min(r));
                            file.write_all(&data[..n]).await?;
                            if let Some(ref mut remaining) = dump_remaining {
                                *remaining -= n;
                            }
                        }
                        for frame in decoder.decode(data)? {
                            if tokio::select! {
                                _ = abort_handle_thread.cancelled() => true,
                                sent = tx.send(Ok(frame)) => sent.is_err(),
                            } {
                                break 'recv_loop;
                            }
                        }
                    } else if let BcBody::ModernMsg(ModernMsg {
                        payload: Some(BcPayloads::BcXml(ref xml)), ..
                    }) = msg.body {
                        // The size acknowledgement describes the stored MP4,
                        // not the number of bytes in the BcMedia transfer.
                        log::debug!("DownloadByTime metadata: {:?}", xml.file_info_list);
                    }
                }
                if let Some(ref mut file) = dump_file {
                    file.flush().await?;
                }
                Ok(())
            }.await;
            drop(sub);
            // Always stop the accepted download, even after a decoder/I/O error.
            let stop_result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                let mut stop = connection.subscribe(MSG_ID_DOWNLOAD_STOP, stop_msg_num).await?;
                stop.send(Bc {
                    meta: BcMeta {
                        msg_id: MSG_ID_DOWNLOAD_STOP,
                        channel_id,
                        msg_num: stop_msg_num,
                        stream_type: 0,
                        response_code: 0,
                        class: 0x6414,
                    },
                    body: BcBody::ModernMsg(ModernMsg { extension: None, payload: None }),
                }).await?;
                let reply = stop.recv().await?;
                if reply.meta.response_code != 200 {
                    return Err(Error::CameraServiceUnavailable {
                        id: MSG_ID_DOWNLOAD_STOP, code: reply.meta.response_code,
                    });
                }
                Ok::<(), Error>(())
            }).await;
            match stop_result {
                Ok(Ok(())) => {}
                Ok(Err(err)) => log::warn!("DownloadByTime: MSG 144 stop failed: {}", err),
                Err(_) => log::warn!("DownloadByTime: MSG 144 stop timed out"),
            }
            result
        });
        // Preserve startup errors for callers that fall back when MSG 143 is
        // rejected, while the task retains ownership of the connection.
        ready_rx.await.map_err(|_| Error::DroppedSubscriber)??;
        Ok(StreamData::from_parts(handle, rx, abort_handle))
    }

    /// Stop replay (MSG 7). Pass channel and file name from the playing file.
    pub async fn replay_stop(&self, name: &str) -> Result<()> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection
            .subscribe(MSG_ID_REPLAY_STOP, msg_num)
            .await?;

        let file_info = FileInfo {
            channel_id: Some(self.channel_id),
            name: Some(name.to_string()),
            ..Default::default()
        };

        let xml = BcXml {
            file_info_list: Some(FileInfoList {
                version: Some(xml_ver()),
                file_info: vec![file_info],
            }),
            ..Default::default()
        };

        let msg = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_REPLAY_STOP,
                channel_id: self.channel_id,
                msg_num,
                stream_type: 0,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::ModernMsg(ModernMsg {
                extension: None,
                payload: Some(BcPayloads::BcXml(xml)),
            }),
        };

        sub.send(msg).await?;
        let msg = sub.recv().await?;

        if msg.meta.response_code == 200 {
            Ok(())
        } else {
            Err(Error::CameraServiceUnavailable {
                id: msg.meta.msg_id,
                code: msg.meta.response_code,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{download_by_time_xml, strip_e1_replay_envelope, ReplayDecoder};
    use crate::bc::xml::{BcXml, ReplayDateTime};
    use crate::bcmedia::model::BcMedia;

    #[test]
    fn download_xml_matches_captured_time_range() {
        let start = ReplayDateTime {
            year: 2026, month: 11, day: 12, hour: 15, minute: 38, second: 14,
        };
        let end = ReplayDateTime { second: 26, ..start.clone() };
        let xml = download_by_time_xml(0, 1, start, end).unwrap();
        let captured = BcXml::try_parse(include_bytes!("../bc/samples/replay_download_by_time.xml")).unwrap();
        assert_eq!(xml, captured);
        assert!(download_by_time_xml(0, 99,
            ReplayDateTime { year: 2026, month: 11, day: 12, hour: 0, minute: 0, second: 0 },
            ReplayDateTime { year: 2026, month: 11, day: 12, hour: 0, minute: 0, second: 1 },
        ).is_err());
    }

    #[test]
    fn replay_decodes_stream_info_and_large_split_iframe() {
        // The first 32 bytes are InfoV2, not a disposable file-size header.
        let info = [
            0x31, 0x30, 0x30, 0x32, 0x20, 0, 0, 0, 0, 6, 0, 0, 0xb0, 1, 0, 0,
            1, 15, 126, 11, 12, 15, 38, 14, 126, 11, 12, 15, 38, 26, 0, 0,
        ];
        let mut decoder = ReplayDecoder::new(true);
        let decoded = decoder.decode(&info).unwrap();
        assert!(matches!(&decoded[0], BcMedia::InfoV2(info) if info.fps == 15));
        let mut iframe = b"00dcH264".to_vec();
        iframe.extend_from_slice(&1024u32.to_le_bytes());
        iframe.extend_from_slice(&4u32.to_le_bytes());
        iframe.extend_from_slice(&100_000u32.to_le_bytes());
        iframe.extend_from_slice(&0u32.to_le_bytes());
        iframe.extend_from_slice(&1_794_498_694u32.to_le_bytes());
        iframe.extend_from_slice(&vec![0x65; 1024]);
        assert!(decoder.decode(&iframe[..600]).unwrap().is_empty());
        let decoded = decoder.decode(&iframe[600..]).unwrap();
        assert!(matches!(&decoded[0], BcMedia::Iframe(frame) if frame.data.len() == 1024));
        assert!(decoder.buffer.is_empty());
    }

    #[test]
    fn replay_passes_mp4_bytes_without_adding_nal_start_codes() {
        let mut decoder = ReplayDecoder::new(true);
        let header = b"\x00\x00\x00\x10ftypisom\x00\x00\x00\x00";
        assert!(matches!(&decoder.decode(header).unwrap()[0],
            BcMedia::RawReplayChunk(data) if data.as_slice() == header));
        // 0x41 can be both a container byte and an H.264 NAL type.
        let continuation = b"\x41container continuation";
        assert!(matches!(&decoder.decode(continuation).unwrap()[0],
            BcMedia::RawReplayChunk(data) if data.as_slice() == continuation));
    }

    #[test]
    fn strip_e1_replay_envelope_returns_payload_after_xml() {
        let prefix_32: Vec<u8> = (0..32).map(|i| i as u8).collect();
        let xml = b"<?xml version=\"1.0\" ?>\n<Extension version=\"1.1\">\n<encryptLen>1024</encryptLen>\n</Extension>\n";
        let payload = b"ftypmp42\x00\x00\x00\x00";
        let body: Vec<u8> = prefix_32.iter().chain(xml).chain(payload.iter()).copied().collect();
        let out = strip_e1_replay_envelope(&body);
        assert_eq!(out, payload as &[u8], "should return only the payload after </Extension>\\n");
    }

    #[test]
    fn strip_e1_replay_envelope_passthrough_when_no_xml() {
        let data = b"ftypmp42\x00\x00\x00\x00";
        assert_eq!(strip_e1_replay_envelope(data), data as &[u8]);
        assert_eq!(strip_e1_replay_envelope(&data[..4]), &data[..4]);
    }

    #[test]
    fn strip_e1_replay_envelope_xml_at_start_no_32_prefix() {
        let xml = b"<?xml version=\"1.0\" ?>\n<Extension version=\"1.1\">\n<encryptLen>1024</encryptLen>\n</Extension>\n";
        let payload = b"ftypmp42\x00\x00\x00\x00";
        let body: Vec<u8> = xml.iter().chain(payload.iter()).copied().collect();
        let out = strip_e1_replay_envelope(&body);
        assert_eq!(out, payload as &[u8], "when body starts with XML (no 32-byte prefix), should return only payload");
    }
}
