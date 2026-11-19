//! Completion is a transport decision, independent of the media decoder.
use crate::bc::{model::*, xml::BcPayloads};

#[derive(Default)]
pub(super) struct ReplaySession {
    file_count: u32,
    section: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Response {
    Data,
    Section,
    End,
}

impl ReplaySession {
    /// Opaque replay class/status words are not reliable service response codes.
    pub(super) fn response_is_status(msg: &Bc) -> bool {
        matches!(msg.meta.class, 0x6414 | 0x0000 | 0x6514 | 0x6614)
            || !matches!(&msg.body, BcBody::ModernMsg(ModernMsg { payload: Some(BcPayloads::Binary(data)), .. }) if !data.is_empty())
            || Self::has_section_metadata(msg)
    }
    pub(super) fn has_section_metadata(msg: &Bc) -> bool {
        matches!(&msg.body,
            BcBody::ModernMsg(ModernMsg { payload: Some(BcPayloads::Binary(data)), .. })
            if data.len() == 32 && data.starts_with(b"10\x20\0"))
    }
    pub(super) fn observe(&mut self, msg: &Bc) -> Response {
        if let BcBody::ModernMsg(ModernMsg {
            payload: Some(BcPayloads::BcXml(xml)),
            ..
        }) = &msg.body
        {
            if let Some(list) = &xml.file_info_list {
                if let Some(count) = list
                    .file_info
                    .iter()
                    .filter_map(|info| info.file_count)
                    .max()
                {
                    self.file_count = count;
                }
            }
        }
        if !Self::response_is_status(msg) {
            return Response::Data;
        }
        if msg.meta.response_code == 300 {
            return Response::End;
        }
        if msg.meta.response_code == 331 {
            let section_metadata = Self::has_section_metadata(msg);
            self.section += 1;
            if section_metadata || self.section < self.file_count {
                return Response::Section;
            }
            return Response::End;
        }
        Response::Data
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bc::xml::{BcXml, FileInfo, FileInfoList};
    fn message(code: u16, payload: Option<BcPayloads>) -> Bc {
        Bc {
            meta: BcMeta {
                msg_id: 143,
                channel_id: 0,
                stream_type: 0,
                msg_num: 0,
                response_code: code,
                class: 0x6414,
            },
            body: BcBody::ModernMsg(ModernMsg {
                extension: None,
                payload,
            }),
        }
    }
    #[test]
    fn hof_section_metadata_does_not_end_download() {
        let mut session = ReplaySession::default();
        let xml = BcXml {
            file_info_list: Some(FileInfoList {
                version: None,
                file_info: vec![FileInfo {
                    file_count: Some(2),
                    ..Default::default()
                }],
            }),
            ..Default::default()
        };
        assert_eq!(
            session.observe(&message(200, Some(BcPayloads::BcXml(xml)))),
            Response::Data
        );
        let mut metadata = vec![0; 32];
        metadata[..4].copy_from_slice(b"10\x20\0");
        assert_eq!(
            session.observe(&message(331, Some(BcPayloads::Binary(metadata)))),
            Response::Section
        );
        assert_eq!(
            session.observe(&message(200, Some(BcPayloads::Binary(vec![1; 1000])))),
            Response::Data
        );
        assert_eq!(session.observe(&message(300, None)), Response::End);
    }
    #[test]
    fn legacy_empty_331_remains_an_end_marker() {
        assert_eq!(
            ReplaySession::default().observe(&message(331, None)),
            Response::End
        );
    }

    #[test]
    fn opaque_binary_status_word_does_not_end_media_reception() {
        for code in [300, 331, 400, 405] {
            let mut msg = message(code, Some(BcPayloads::Binary(vec![0x65; 100])));
            msg.meta.class = 0x6afb;
            assert!(!ReplaySession::response_is_status(&msg));
            assert_eq!(ReplaySession::default().observe(&msg), Response::Data);
        }
    }

    #[test]
    fn file_count_is_parsed_from_file_info_and_bounds_empty_section_markers() {
        use crate::bc::xml::BcXml;
        let xml = BcXml::try_parse(&b"<?xml version=\"1.0\"?><body><FileInfoList version=\"1.1\"><FileInfo><FileCount>2</FileCount><sizeL>658977</sizeL></FileInfo></FileInfoList></body>"[..]).unwrap();
        let mut session = ReplaySession::default();
        assert_eq!(
            session.observe(&message(200, Some(BcPayloads::BcXml(xml)))),
            Response::Data
        );
        assert_eq!(session.observe(&message(331, None)), Response::Section);
        assert_eq!(session.observe(&message(331, None)), Response::End);
    }
}
