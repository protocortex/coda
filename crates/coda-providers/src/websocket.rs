// SPDX-License-Identifier: AGPL-3.0-only

//! The small slice of RFC 6455 a client needs to talk to a local Codex app-server.
//!
//! Only unfragmented-or-fragmented text, ping, pong and close are understood.
//! The `Sec-WebSocket-Accept` header is not checked: the peer is a child process
//! reached over a private socket, so the check would defend against nothing.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};

use crate::jsonl::MAX_LINE_BYTES;

/// Longest message accepted, after reassembling fragments.
pub(crate) const MAX_MESSAGE_BYTES: usize = MAX_LINE_BYTES;
/// Longest HTTP response head read during the upgrade.
pub(crate) const MAX_HEAD_BYTES: usize = 16 * 1024;
const OP_CONTINUATION: u8 = 0x0;
const OP_TEXT: u8 = 0x1;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
pub(crate) const OP_PONG: u8 = 0xA;
const FIN: u8 = 0x80;
const RESERVED_BITS: u8 = 0x70;
const MASKED: u8 = 0x80;
const MAX_CONTROL_BYTES: usize = 125;
const LEN_16_BIT: u8 = 126;
const LEN_64_BIT: u8 = 127;
const HEAD_END: &[u8] = b"\r\n\r\n";
const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Why the byte stream is not a valid WebSocket exchange.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WsError {
    /// A frame or message exceeds the size limit.
    #[error("a WebSocket frame or message is over the {MAX_MESSAGE_BYTES} byte limit")]
    TooLarge,
    /// The peer broke a framing rule.
    #[error("the WebSocket peer broke a framing rule: {0}")]
    Violation(&'static str),
    /// A text message is not valid UTF-8.
    #[error("a WebSocket text message is not valid UTF-8")]
    BadText,
}

/// One complete unit received from the peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Incoming {
    Text(String),
    Ping(Vec<u8>),
    Pong,
    Close,
}

/// The upgrade request to send, with a fresh random key.
pub(crate) fn upgrade_request() -> String {
    let key = base64(&random_bytes::<16>());
    format!(
        "GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )
}

/// Splits a buffer at the end of the HTTP head: the status code and the bytes after the head.
/// `None` means the head is not complete yet.
pub(crate) fn parse_upgrade_response(buffer: &[u8]) -> Result<Option<(u16, &[u8])>, WsError> {
    let Some(end) = buffer.windows(HEAD_END.len()).position(|w| w == HEAD_END) else {
        return if buffer.len() > MAX_HEAD_BYTES {
            Err(WsError::TooLarge)
        } else {
            Ok(None)
        };
    };
    let head = String::from_utf8_lossy(&buffer[..end]);
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .ok_or(WsError::Violation(
            "the upgrade response has no status code",
        ))?;
    Ok(Some((status, &buffer[end + HEAD_END.len()..])))
}

/// A masked client frame; clients must mask, servers must not.
pub(crate) fn encode_client_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![FIN | opcode];
    let len = payload.len();
    if let Ok(short) = u8::try_from(len)
        && usize::from(short) < usize::from(LEN_16_BIT)
    {
        frame.push(MASKED | short);
    } else if let Ok(medium) = u16::try_from(len) {
        frame.push(MASKED | LEN_16_BIT);
        frame.extend_from_slice(&medium.to_be_bytes());
    } else {
        frame.push(MASKED | LEN_64_BIT);
        frame.extend_from_slice(&(len as u64).to_be_bytes());
    }
    let mask = random_bytes::<4>();
    frame.extend_from_slice(&mask);
    frame.extend(
        payload
            .iter()
            .enumerate()
            .map(|(i, byte)| byte ^ mask[i % 4]),
    );
    frame
}

pub(crate) fn text_frame(text: &str) -> Vec<u8> {
    encode_client_frame(OP_TEXT, text.as_bytes())
}

pub(crate) fn pong_frame(payload: &[u8]) -> Vec<u8> {
    encode_client_frame(OP_PONG, payload)
}

/// The reply to a server close, per RFC 6455 section 5.5.1.
pub(crate) fn close_frame() -> Vec<u8> {
    encode_client_frame(OP_CLOSE, &[])
}

/// One parsed frame and the buffer bytes it occupied.
struct Frame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
    used: usize,
}

/// Reassembles server frames from raw bytes.
#[derive(Debug, Default)]
pub(crate) struct FrameReader {
    buffer: Vec<u8>,
    message: Vec<u8>,
    in_message: bool,
}

impl FrameReader {
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// The next complete message or control frame, or `None` when more bytes are needed.
    pub(crate) fn next(&mut self) -> Result<Option<Incoming>, WsError> {
        while let Some(Frame {
            fin,
            opcode,
            payload,
            used,
        }) = self.frame()?
        {
            self.buffer.drain(..used);
            match opcode {
                OP_PING => return Ok(Some(Incoming::Ping(payload))),
                OP_PONG => return Ok(Some(Incoming::Pong)),
                OP_CLOSE => return Ok(Some(Incoming::Close)),
                OP_TEXT | OP_CONTINUATION => {
                    if (opcode == OP_TEXT) == self.in_message {
                        return Err(WsError::Violation("fragments are out of order"));
                    }
                    if self.message.len() + payload.len() > MAX_MESSAGE_BYTES {
                        return Err(WsError::TooLarge);
                    }
                    self.message.extend_from_slice(&payload);
                    self.in_message = !fin;
                    if fin {
                        let text = String::from_utf8(std::mem::take(&mut self.message))
                            .map_err(|_| WsError::BadText)?;
                        return Ok(Some(Incoming::Text(text)));
                    }
                }
                _ => unreachable!("frame() rejects every other opcode"),
            }
        }
        Ok(None)
    }

    /// Parses one frame off the front of the buffer without consuming it.
    fn frame(&self) -> Result<Option<Frame>, WsError> {
        let buf = &self.buffer;
        let [first, second, ..] = buf.as_slice() else {
            return Ok(None);
        };
        if first & RESERVED_BITS != 0 {
            return Err(WsError::Violation("reserved bits are set"));
        }
        if second & MASKED != 0 {
            return Err(WsError::Violation("a server frame is masked"));
        }
        let (fin, opcode) = (first & FIN != 0, first & 0x0F);
        if !matches!(
            opcode,
            OP_CONTINUATION | OP_TEXT | OP_CLOSE | OP_PING | OP_PONG
        ) {
            return Err(WsError::Violation("an unsupported opcode"));
        }
        let (len, head) = match second & !MASKED {
            LEN_16_BIT => match buf.get(2..4) {
                Some(b) => (u64::from(u16::from_be_bytes([b[0], b[1]])), 4),
                None => return Ok(None),
            },
            LEN_64_BIT => match buf.get(2..10) {
                Some(b) => (u64::from_be_bytes(b.try_into().unwrap_or_default()), 10),
                None => return Ok(None),
            },
            short => (u64::from(short), 2),
        };
        let len = usize::try_from(len).map_err(|_| WsError::TooLarge)?;
        if len > MAX_MESSAGE_BYTES {
            return Err(WsError::TooLarge);
        }
        if opcode & 0x8 != 0 && (!fin || len > MAX_CONTROL_BYTES) {
            return Err(WsError::Violation(
                "a control frame is fragmented or too long",
            ));
        }
        let Some(payload) = buf.get(head..head + len) else {
            return Ok(None);
        };
        Ok(Some(Frame {
            fin,
            opcode,
            payload: payload.to_vec(),
            used: head + len,
        }))
    }
}

/// Unpredictable-enough bytes for the key and masks, from the std hasher's random seed.
fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0_u8; N];
    for chunk in out.chunks_mut(8) {
        let word = RandomState::new().build_hasher().finish().to_le_bytes();
        chunk.copy_from_slice(&word[..chunk.len()]);
    }
    out
}

fn base64(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .fold(0_u32, |acc, b| (acc << 8) | u32::from(*b))
            << (8 * (3 - chunk.len()));
        for i in 0..=chunk.len() {
            out.push(char::from(BASE64[((n >> (18 - 6 * i)) & 0x3F) as usize]));
        }
        out.extend(std::iter::repeat_n('=', 3 - chunk.len()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_frame(first: u8, payload: &[u8]) -> Vec<u8> {
        let mut frame = vec![first, u8::try_from(payload.len()).unwrap()];
        frame.extend_from_slice(payload);
        frame
    }

    #[test]
    fn base64_matches_the_rfc_4648_vectors() {
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"f"), "Zg==");
    }

    #[test]
    fn client_frames_are_masked_and_decode_back() {
        // Arrange
        let frame = encode_client_frame(OP_TEXT, b"hello");

        // Act
        let mask = &frame[2..6];
        let payload: Vec<u8> = frame[6..]
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ mask[i % 4])
            .collect();

        // Assert
        assert_eq!(frame[1] & MASKED, MASKED);
        assert_eq!(payload, b"hello");
    }

    #[test]
    fn fragments_are_reassembled_and_split_input_is_held() {
        // Arrange
        let mut reader = FrameReader::default();
        let mut bytes = server_frame(OP_TEXT, b"frag");
        bytes.extend(server_frame(FIN | OP_CONTINUATION, b"ment"));

        // Act
        reader.feed(&bytes[..3]);
        let early = reader.next().unwrap();
        reader.feed(&bytes[3..]);

        // Assert
        assert_eq!(early, None);
        assert_eq!(
            reader.next().unwrap(),
            Some(Incoming::Text("fragment".to_owned()))
        );
    }

    #[test]
    fn framing_violations_are_errors() {
        let cases: [(&[u8], WsError); 3] = [
            (
                &[FIN | OP_TEXT, MASKED],
                WsError::Violation("a server frame is masked"),
            ),
            (
                &[FIN | 0x40 | OP_TEXT, 0],
                WsError::Violation("reserved bits are set"),
            ),
            (
                &[FIN | OP_TEXT, LEN_64_BIT, 0, 0, 0, 0, 0xFF, 0, 0, 0],
                WsError::TooLarge,
            ),
        ];
        for (bytes, expected) in cases {
            // Arrange
            let mut reader = FrameReader::default();
            reader.feed(bytes);

            // Act
            let result = reader.next();

            // Assert
            assert_eq!(result, Err(expected));
        }
    }

    fn next_error(bytes: &[u8]) -> WsError {
        let mut reader = FrameReader::default();
        reader.feed(bytes);
        reader.next().unwrap_err()
    }

    #[test]
    fn unsupported_opcodes_are_rejected_from_the_header_alone() {
        for opcode in [0x2_u8, 0x3, 0x7, 0xB, 0xF] {
            // Arrange: a header announcing a large payload that never arrives
            let bytes = [FIN | opcode, LEN_16_BIT, 0xFF, 0xFF];

            // Act
            let error = next_error(&bytes);

            // Assert
            assert_eq!(error, WsError::Violation("an unsupported opcode"));
        }
    }

    #[test]
    fn a_control_frame_over_125_bytes_is_a_violation() {
        // Arrange
        let mut bytes = vec![FIN | OP_PING, LEN_16_BIT, 0, 126];
        bytes.extend(vec![0_u8; 126]);

        // Act
        let error = next_error(&bytes);

        // Assert
        assert_eq!(
            error,
            WsError::Violation("a control frame is fragmented or too long")
        );
    }

    #[test]
    fn a_fragmented_control_frame_is_a_violation() {
        // Arrange
        let bytes = server_frame(OP_PING, b"");

        // Act
        let error = next_error(&bytes);

        // Assert
        assert_eq!(
            error,
            WsError::Violation("a control frame is fragmented or too long")
        );
    }

    #[test]
    fn a_continuation_with_no_start_is_a_violation() {
        // Arrange
        let bytes = server_frame(FIN | OP_CONTINUATION, b"x");

        // Act
        let error = next_error(&bytes);

        // Assert
        assert_eq!(error, WsError::Violation("fragments are out of order"));
    }

    #[test]
    fn a_text_message_that_is_not_utf8_is_bad_text() {
        // Arrange
        let bytes = server_frame(FIN | OP_TEXT, &[0xFF, 0xFE]);

        // Act
        let error = next_error(&bytes);

        // Assert
        assert_eq!(error, WsError::BadText);
    }

    #[test]
    fn a_ping_between_fragments_is_delivered_and_the_message_survives() {
        // Arrange
        let mut reader = FrameReader::default();
        let mut bytes = server_frame(OP_TEXT, b"ab");
        bytes.extend(server_frame(FIN | OP_PING, b"p"));
        bytes.extend(server_frame(FIN | OP_CONTINUATION, b"cd"));
        reader.feed(&bytes);

        // Act
        let first = reader.next().unwrap();
        let second = reader.next().unwrap();

        // Assert
        assert_eq!(first, Some(Incoming::Ping(b"p".to_vec())));
        assert_eq!(second, Some(Incoming::Text("abcd".to_owned())));
    }

    #[test]
    fn the_close_reply_is_a_masked_empty_close_frame() {
        // Arrange / Act
        let frame = close_frame();

        // Assert
        assert_eq!(frame[0], FIN | OP_CLOSE);
        assert_eq!(frame[1], MASKED);
        assert_eq!(frame.len(), 6);
    }
}
