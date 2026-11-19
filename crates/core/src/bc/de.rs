use super::model::*;
use crate::Error;
use bytes::{Buf, BytesMut};
use log::*;
use nom::{
    bytes::streaming::take, combinator::*, error::context as error_context, number::streaming::*,
    Parser,
};

type IResult<I, O, E = nom_language::error::VerboseError<I>> = Result<(I, O), nom::Err<E>>;

impl Bc {
    /// Returns Ok(deserialized data, the amount of data consumed)
    /// Can then use this as the amount that should be remove from a buffer
    pub(crate) fn deserialize(context: &BcContext, buf: &mut BytesMut) -> Result<Bc, Error> {
        let parser = |input| bc_msg(context, input);
        let (result, amount) = match consumed(parser).parse(&buf[..]) {
            Ok((_, (parsed_buff, result))) => Ok((result, parsed_buff.len())),
            Err(e) => Err(Error::from(e)),
        }?;

        buf.advance(amount);
        Ok(result)
    }
}

fn bc_msg<'a>(context: &BcContext, buf: &'a [u8]) -> IResult<&'a [u8], Bc> {
    let (buf, header) = bc_header(buf)?;
    let (buf, body) = bc_body(context, &header, buf)?;

    let bc = Bc {
        meta: header.to_meta(),
        body,
    };

    Ok((buf, bc))
}

fn bc_body<'a>(context: &BcContext, header: &BcHeader, buf: &'a [u8]) -> IResult<&'a [u8], BcBody> {
    if header.is_modern() {
        let (buf, body) = bc_modern_msg(context, header, buf)?;
        Ok((buf, BcBody::ModernMsg(body)))
    } else {
        let (buf, body) = match header.msg_id {
            MSG_ID_LOGIN => bc_legacy_login_msg(buf)?,
            _ => (buf, LegacyMsg::UnknownMsg),
        };
        Ok((buf, BcBody::LegacyMsg(body)))
    }
}

fn hex32<'a>() -> impl Parser<&'a [u8], Output = String, Error = crate::NomErrorType<'a>> {
    map_res(take(32usize), |slice: &'a [u8]| {
        String::from_utf8(slice.to_vec())
    })
}

fn bc_legacy_login_msg(buf: &'_ [u8]) -> IResult<&'_ [u8], LegacyMsg> {
    let (buf, username) = hex32().parse(buf)?;
    let (buf, password) = hex32().parse(buf)?;

    Ok((buf, LegacyMsg::LoginMsg { username, password }))
}

fn bc_modern_msg<'a>(
    context: &BcContext,
    header: &BcHeader,
    buf: &'a [u8],
) -> IResult<&'a [u8], ModernMsg> {
    use nom::{
        error::{ContextError, ErrorKind, ParseError},
        Err,
    };

    fn make_error<I, E>(input: I, ctx: &'static str, kind: ErrorKind) -> E
    where
        I: std::marker::Copy,
        E: ParseError<I> + ContextError<I>,
    {
        E::add_context(input, ctx, E::from_error_kind(input, kind))
    }

    // If missing payload_offset treat all as payload
    let ext_len = header.payload_offset.unwrap_or_default();

    let payload_len = header.body_len.checked_sub(ext_len).ok_or_else(|| {
        Err::Error(make_error(
            buf,
            "Payload offset exceeds body length",
            ErrorKind::Verify,
        ))
    })?;
    let (buf, ext_buf) = take(ext_len).parse(buf)?;
    let (buf, payload_buf) = take(payload_len).parse(buf)?;

    let decrypted;
    let processed_ext_buf = match context.get_encrypted() {
        EncryptionProtocol::Unencrypted => ext_buf,
        encryption_protocol => {
            decrypted = encryption_protocol.decrypt(header.channel_id as u32, ext_buf);
            &decrypted
        }
    };

    let mut in_binary = false;
    let mut encrypted_len = None;
    // Now we'll take the buffer that Nom gave a ref to and parse it.
    let extension = if ext_len > 0 {
        if context.debug {
            println!(
                "Extension Txt: {:?}",
                String::from_utf8(processed_ext_buf.to_vec()).unwrap_or("Not Text".to_string())
            );
        }
        // Apply the XML parse function, but throw away the reference to decrypted in the Ok and
        // Err case. This error-error-error thing is the same idiom Nom uses internally.
        let parsed = Extension::try_parse(processed_ext_buf).map_err(|_| {
            log::error!("Extension buffer: {:?}", processed_ext_buf);
            Err::Error(make_error(
                buf,
                "Unable to parse Extension XML",
                ErrorKind::MapRes,
            ))
        })?;
        if let Extension {
            binary_data: Some(1),
            encrypt_len,
            ..
        } = parsed
        {
            // In binary so tell the current context that we need to treat the payload as binary
            in_binary = true;
            encrypted_len = encrypt_len;
        }
        Some(parsed)
    } else {
        None
    };

    // Now to handle the payload block
    // This block can either be xml or binary depending on what the message expects.
    // For our purposes we use try_parse and if all xml based parsers fail we treat
    // As binary
    let payload;
    if payload_len > 0 {
        // Extract remainder of message as binary, if it exists
        const UNENCRYPTED: EncryptionProtocol = EncryptionProtocol::Unencrypted;
        const BC_ENCRYPTED: EncryptionProtocol = EncryptionProtocol::BCEncrypt;
        let encryption_protocol = match header {
            BcHeader {
                msg_id: 1,
                response_code,
                ..
            } if (response_code >> 8) & 0xff == 0xdd => {
                // 0xdd means we are setting the encryption method
                // Durig login, the max encryption is BcEncrypt since
                // the nonce has not been exchanged yet
                match response_code & 0xff {
                    0x00 => &UNENCRYPTED,
                    _ => &BC_ENCRYPTED,
                }
            }
            BcHeader { msg_id: 1, .. } => {
                match &context.get_encrypted() {
                    EncryptionProtocol::Aes { .. } | EncryptionProtocol::FullAes { .. } => {
                        // During login max is BcEncrypt
                        &BC_ENCRYPTED
                    }
                    n => *n,
                }
            }
            _ => context.get_encrypted(),
        };

        if context.in_bin_mode.borrow().contains(&(header.msg_num)) || in_binary {
            payload = match (context.get_encrypted(), encrypted_len) {
                (EncryptionProtocol::FullAes { .. }, Some(encrypted_len)) => {
                    // encryptLen describes an encrypted prefix, not the total
                    // payload length. Continuation bytes remain unencrypted.
                    let encrypted_len = encrypted_len as usize;
                    if encrypted_len > payload_buf.len() {
                        return Err(Err::Error(make_error(
                            buf,
                            "encryptLen exceeds payload length",
                            ErrorKind::Verify,
                        )));
                    }
                    let mut data = encryption_protocol.decrypt(
                        header.channel_id as u32,
                        &payload_buf[..encrypted_len],
                    );
                    data.extend_from_slice(&payload_buf[encrypted_len..]);
                    Some(BcPayloads::Binary(data))
                }
                _ => Some(BcPayloads::Binary(payload_buf.to_vec())),
            };
        } else {
            let processed_payload_buf =
                encryption_protocol.decrypt(header.channel_id as u32, payload_buf);
            if context.debug {
                println!(
                    "Payload Txt: {:?}",
                    String::from_utf8(processed_payload_buf.to_vec())
                        .unwrap_or("Not Text".to_string())
                );
            }
            let xml = BcXml::try_parse(processed_payload_buf.as_slice()).map_err(|e| {
                error!("header.msg_id: {}", header.msg_id);
                error!(
                    "processed_payload_buf: {:X?}::{:?}",
                    processed_payload_buf,
                    std::str::from_utf8(&processed_payload_buf)
                );
                log::error!("e: {:?}", e);
                Err::Error(make_error(
                    buf,
                    "Unable to parse Payload XML",
                    ErrorKind::MapRes,
                ))
            })?;
            payload = Some(BcPayloads::BcXml(xml));
        }
    } else {
        payload = None;
    }

    Ok((buf, ModernMsg { extension, payload }))
}

fn bc_header(buf: &[u8]) -> IResult<&[u8], BcHeader> {
    let (buf, _magic) = error_context(
        "Magic invalid",
        verify(le_u32, |x| *x == MAGIC_HEADER || *x == MAGIC_HEADER_REV),
    )
    .parse(buf)?;
    let (buf, msg_id) = error_context("MsgID missing", le_u32).parse(buf)?;
    let (buf, body_len) = error_context("BodyLen missing", le_u32).parse(buf)?;
    let (buf, channel_id) = error_context("ChannelID missing", le_u8).parse(buf)?;
    let (buf, stream_type) = error_context("StreamType missing", le_u8).parse(buf)?;
    let (buf, msg_num) = error_context("MsgNum missing", le_u16).parse(buf)?;
    let (buf, (response_code, class)) =
        error_context("ResponseCode missing", (le_u16, le_u16)).parse(buf)?;

    let (buf, payload_offset) = error_context(
        "Payload Offset is missing",
        cond(has_payload_offset(class, msg_id), le_u32),
    )
    .parse(buf)?;

    Ok((
        buf,
        BcHeader {
            body_len,
            msg_id,
            channel_id,
            stream_type,
            msg_num,
            response_code,
            class,
            payload_offset,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bc::xml::*;
    use assert_matches::assert_matches;
    use env_logger::Env;
    use std::convert::TryInto;

    #[test]
    fn fragmented_login_waits_for_more_data_without_consuming_input() {
        let context = BcContext::new_with_encryption(EncryptionProtocol::BCEncrypt);
        crate::assert_fragmented_parse(
            include_bytes!("samples/model_sample_modern_login.bin"),
            |buf| Bc::deserialize(&context, buf),
        );
    }

    #[test]
    fn replay_opaque_class_keeps_offset_and_unencrypted_tail() {
        // Captured Argus 4 Pro header layout, neutral key and synthetic media.
        // Includes an encrypted prefix followed by an unencrypted continuation.
        let sample = include_bytes!("samples/replay_partial_aes.bin");
        let context = BcContext::new_with_encryption(
            EncryptionProtocol::full_aes(*b"test replay key!"),
        );
        let mut buf = BytesMut::from(&sample[..]);
        buf.extend_from_slice(sample);
        let mut expected = b"01dcH264".to_vec();
        expected.extend_from_slice(&[0; 24]);
        expected.extend_from_slice(b"unencrypted continuation");
        for _ in 0..2 {
            let msg = Bc::deserialize(&context, &mut buf).unwrap();
            assert_eq!(msg.meta.class, 0x6afb);
            assert_eq!(msg.meta.response_code, 0x5070);
            match msg.body {
                BcBody::ModernMsg(ModernMsg {
                    extension: Some(ext), payload: Some(BcPayloads::Binary(data)),
                }) => {
                    assert_eq!(ext.encrypt_len, Some(32));
                    assert_eq!(data, expected);
                }
                other => panic!("Expected decrypted binary replay: {:?}", other),
            }
        }
        assert!(buf.is_empty(), "Each 24-byte header must be consumed completely");
    }

    #[test]
    fn replay_rejects_encrypt_len_larger_than_payload() {
        let mut sample = include_bytes!("samples/replay_partial_aes.bin").to_vec();
        let ext_len = u32::from_le_bytes(sample[20..24].try_into().unwrap()) as usize;
        sample.truncate(24 + ext_len + 16);
        sample[8..12].copy_from_slice(&((ext_len + 16) as u32).to_le_bytes());
        let context = BcContext::new_with_encryption(
            EncryptionProtocol::full_aes(*b"test replay key!"),
        );
        assert!(matches!(Bc::deserialize(&context, &mut BytesMut::from(&sample[..])),
            Err(Error::NomError(_))));
    }

    #[test]
    fn replay_rejects_payload_offset_larger_than_body() {
        let mut sample = include_bytes!("samples/replay_partial_aes.bin").to_vec();
        sample[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
        let context = BcContext::new_with_encryption(EncryptionProtocol::Unencrypted);
        assert!(matches!(Bc::deserialize(&context, &mut BytesMut::from(&sample[..])),
            Err(Error::NomError(_))));
    }

    fn init() {
        let _ = env_logger::Builder::from_env(Env::default().default_filter_or("info"))
            .is_test(true)
            .try_init();
    }

    #[test]
    fn test_bc_modern_login() {
        init();

        let sample = include_bytes!("samples/model_sample_modern_login.bin");

        let context = BcContext::new_with_encryption(EncryptionProtocol::BCEncrypt);

        let (buf, header) = bc_header(&sample[..]).unwrap();
        let (_, body) = bc_body(&context, &header, buf).unwrap();
        assert_eq!(header.msg_id, 1);
        assert_eq!(header.body_len, 145);
        assert_eq!(header.channel_id, 0);
        assert_eq!(header.stream_type, 0);
        assert_eq!(header.payload_offset, None);
        assert_eq!(header.response_code, 0xdd01);
        assert_eq!(header.class, 0x6614);
        match body {
            BcBody::ModernMsg(ModernMsg {
                payload:
                    Some(BcPayloads::BcXml(BcXml {
                        encryption: Some(encryption),
                        ..
                    })),
                ..
            }) => assert_eq!(encryption.nonce, "9E6D1FCB9E69846D"),
            _ => panic!(),
        }
    }

    #[test]
    // This is an 0xdd03 encryption from an Argus 2
    fn test_03_enc_login() {
        init();

        let sample = include_bytes!("samples/battery_enc.bin");

        let context = BcContext::new_with_encryption(EncryptionProtocol::BCEncrypt);

        let (buf, header) = bc_header(&sample[..]).unwrap();
        let (_, body) = bc_body(&context, &header, buf).unwrap();
        assert_eq!(header.msg_id, 1);
        assert_eq!(header.body_len, 175);
        assert_eq!(header.channel_id, 0);
        assert_eq!(header.stream_type, 0);
        assert_eq!(header.payload_offset, None);
        assert_eq!(header.response_code, 0xdd03);
        assert_eq!(header.class, 0x6614);
        match body {
            BcBody::ModernMsg(ModernMsg {
                payload:
                    Some(BcPayloads::BcXml(BcXml {
                        encryption: Some(encryption),
                        ..
                    })),
                ..
            }) => assert_eq!(encryption.nonce, "0-AhnEZyUg6eKrJFIWgXPF"),
            _ => panic!(),
        }
    }

    #[test]
    fn test_bc_legacy_login() {
        init();

        let sample = include_bytes!("samples/model_sample_legacy_login.bin");

        let context = BcContext::new_with_encryption(EncryptionProtocol::BCEncrypt);

        let (buf, header) = bc_header(&sample[..]).unwrap();
        let (_, body) = bc_body(&context, &header, buf).unwrap();
        assert_eq!(header.msg_id, 1);
        assert_eq!(header.body_len, 1836);
        assert_eq!(header.channel_id, 0);
        assert_eq!(header.stream_type, 0);
        assert_eq!(header.payload_offset, None);
        assert_eq!(header.response_code, 0xdc01);
        assert_eq!(header.class, 0x6514);
        match body {
            BcBody::LegacyMsg(LegacyMsg::LoginMsg { username, password }) => {
                assert_eq!(username, "21232F297A57A5A743894A0E4A801FC\0");
                assert_eq!(password, EMPTY_LEGACY_PASSWORD);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn test_bc_modern_login_failed() {
        init();

        let sample = include_bytes!("samples/modern_login_failed.bin");

        let context = BcContext::new_with_encryption(EncryptionProtocol::BCEncrypt);

        let (buf, header) = bc_header(&sample[..]).unwrap();
        let (_, body) = bc_body(&context, &header, buf).unwrap();
        assert_eq!(header.msg_id, 1);
        assert_eq!(header.body_len, 0);
        assert_eq!(header.channel_id, 0);
        assert_eq!(header.stream_type, 0);
        assert_eq!(header.payload_offset, Some(0x0));
        assert_eq!(header.response_code, 0x190); // 400
        assert_eq!(header.class, 0x0000);
        match body {
            BcBody::ModernMsg(ModernMsg {
                extension: None,
                payload: None,
            }) => {}
            _ => panic!(),
        }
    }

    #[test]
    fn test_bc_modern_login_success() {
        init();

        let sample = include_bytes!("samples/modern_login_success.bin");

        let context = BcContext::new_with_encryption(EncryptionProtocol::BCEncrypt);

        let (buf, header) = bc_header(&sample[..]).unwrap();
        let (_, body) = bc_body(&context, &header, buf).unwrap();
        assert_eq!(header.msg_id, 1);
        assert_eq!(header.body_len, 2949);
        assert_eq!(header.channel_id, 0);
        assert_eq!(header.stream_type, 0);
        assert_eq!(header.payload_offset, Some(0x0));
        assert_eq!(header.response_code, 0xc8); // 200
        assert_eq!(header.class, 0x0000);

        // Previously, we were not handling payload_offset == 0 (no bin offset) correctly.
        // Test that we decoded XML and no binary.
        match body {
            BcBody::ModernMsg(ModernMsg {
                extension: None,
                payload: Some(_),
            }) => {}
            _ => panic!(),
        }
    }

    #[test]
    fn test_bc_binary_mode() {
        init();

        let sample1 = include_bytes!("samples/modern_video_start1.bin");
        let sample2 = include_bytes!("samples/modern_video_start2.bin");

        let context = BcContext::new_with_encryption(EncryptionProtocol::BCEncrypt);

        let msg1 = Bc::deserialize(&context, &mut BytesMut::from(&sample1[..])).unwrap();
        match msg1.body {
            BcBody::ModernMsg(ModernMsg {
                extension:
                    Some(Extension {
                        binary_data: Some(1),
                        ..
                    }),
                payload: Some(BcPayloads::Binary(bin)),
            }) => {
                assert_eq!(bin.len(), 32);
            }
            _ => panic!(),
        }

        context.binary_on(msg1.meta.msg_num);
        let msg2 = Bc::deserialize(&context, &mut BytesMut::from(&sample2[..])).unwrap();
        match msg2.body {
            BcBody::ModernMsg(ModernMsg {
                extension: None,
                payload: Some(BcPayloads::Binary(bin)),
            }) => {
                assert_eq!(bin.len(), 30344);
            }
            _ => panic!(),
        }
    }

    #[test]
    // B800 seems to have a different header to the E1 and swann cameras
    // the stream_type and message_num do not seem to set in the official clients
    //
    // They also have extra streams
    fn test_bc_b800_externstream() {
        init();

        let sample = include_bytes!("samples/xml_externstream_b800.bin");

        let context = BcContext::new_with_encryption(EncryptionProtocol::BCEncrypt);

        let e = Bc::deserialize(&context, &mut BytesMut::from(&sample[..]));
        assert_matches!(
            e,
            Ok(Bc {
                meta:
                    BcMeta {
                        msg_id: 3,
                        channel_id: 0x8c,
                        stream_type: 0,
                        response_code: 0,
                        msg_num: 0,
                        class: 0x6414,
                    },
                body:
                    BcBody::ModernMsg(ModernMsg {
                        extension: None,
                        payload:
                            Some(BcPayloads::BcXml(BcXml {
                                preview:
                                    Some(Preview {
                                        version,
                                        channel_id: 0,
                                        handle: 1024,
                                        stream_type,
                                    }),
                                ..
                            })),
                    }),
            }) if version == "1.1" && stream_type == Some("externStream".to_string())
        );
    }

    #[test]
    // B800 seems to have a different header to the E1 and swann cameras
    // the stream_type and message_num do not seem to set in the official clients
    //
    // They also have extra streams
    fn test_bc_b800_substream() {
        init();

        let sample = include_bytes!("samples/xml_substream_b800.bin");

        let context = BcContext::new_with_encryption(EncryptionProtocol::BCEncrypt);

        let e = Bc::deserialize(&context, &mut BytesMut::from(&sample[..]));
        assert_matches!(
            e,
            Ok(Bc {
                meta:
                    BcMeta {
                        msg_id: 3,
                        channel_id: 143,
                        stream_type: 0,
                        response_code: 0,
                        msg_num: 0,
                        class: 0x6414,
                    },
                body:
                    BcBody::ModernMsg(ModernMsg {
                        extension: None,
                        payload:
                            Some(BcPayloads::BcXml(BcXml {
                                preview:
                                    Some(Preview {
                                        version,
                                        channel_id: 0,
                                        handle: 256,
                                        stream_type,
                                    }),
                                ..
                            })),
                    }),
            }) if version == "1.1" && stream_type == Some("subStream".to_string())
        );
    }

    #[test]
    // B800 seems to have a different header to the E1 and swann cameras
    // the stream_type and message_num do not seem to set in the official clients
    //
    // They also have extra streams
    fn test_bc_b800_mainstream() {
        init();

        let sample = include_bytes!("samples/xml_mainstream_b800.bin");

        let context = BcContext::new_with_encryption(EncryptionProtocol::BCEncrypt);

        let e = Bc::deserialize(&context, &mut BytesMut::from(&sample[..]));
        assert_matches!(
            e,
            Ok(Bc {
                meta:
                    BcMeta {
                        msg_id: 3,
                        channel_id: 138,
                        stream_type: 0,
                        response_code: 0,
                        msg_num: 0,
                        class: 0x6414,
                    },
                body:
                    BcBody::ModernMsg(ModernMsg {
                        extension: None,
                        payload:
                            Some(BcPayloads::BcXml(BcXml {
                                preview:
                                    Some(Preview {
                                        version,
                                        channel_id: 0,
                                        handle: 0,
                                        stream_type,
                                    }),
                                ..
                            })),
                    }),
            }) if version == "1.1" && stream_type == Some("mainStream".to_string())
        );
    }
}
