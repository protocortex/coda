// SPDX-License-Identifier: AGPL-3.0-only

//! Length-prefixed framing: a 4-byte big-endian length, then the payload.

use crate::jsonrpc::Message;

/// Largest payload a frame may carry (16 MiB).
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

const HEADER_LEN: usize = size_of::<u32>();
const MAX_BUFFER_BYTES: usize = HEADER_LEN + MAX_FRAME_BYTES;

/// Failures while framing or decoding messages.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CodecError {
    /// A frame payload exceeds [`MAX_FRAME_BYTES`].
    #[error("frame of {size} bytes exceeds the maximum of {max} bytes")]
    FrameTooLarge {
        /// Announced or actual payload size in bytes.
        size: usize,
        /// The enforced limit in bytes.
        max: usize,
    },
    /// Buffering more bytes would exceed the decoder's capacity.
    #[error("decoder buffer of {buffered} bytes cannot take {incoming} more, the maximum is {max}")]
    BufferOverflow {
        /// Bytes already buffered.
        buffered: usize,
        /// Bytes the rejected push carried.
        incoming: usize,
        /// The enforced limit in bytes.
        max: usize,
    },
    /// A frame payload is not a valid JSON-RPC message.
    #[error("frame payload is not a valid JSON-RPC message: {0}")]
    InvalidJson(#[from] serde_json::Error),
}

/// Prefixes `payload` with its length.
///
/// # Errors
/// Returns [`CodecError::FrameTooLarge`] if `payload` exceeds [`MAX_FRAME_BYTES`].
pub fn encode_frame(payload: &[u8]) -> Result<Vec<u8>, CodecError> {
    let too_large = || CodecError::FrameTooLarge {
        size: payload.len(),
        max: MAX_FRAME_BYTES,
    };
    if payload.len() > MAX_FRAME_BYTES {
        return Err(too_large());
    }
    let len = u32::try_from(payload.len()).map_err(|_| too_large())?;
    let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

/// Serializes `message` to JSON and frames it.
///
/// # Errors
/// Returns [`CodecError::InvalidJson`] if serialization fails, or
/// [`CodecError::FrameTooLarge`] if the JSON exceeds [`MAX_FRAME_BYTES`].
pub fn encode_message(message: &Message) -> Result<Vec<u8>, CodecError> {
    encode_frame(&serde_json::to_vec(message)?)
}

/// Parses a frame payload as a JSON-RPC message.
///
/// # Errors
/// Returns [`CodecError::InvalidJson`] if the payload is not a valid message,
/// including an empty payload.
pub fn decode_message(payload: &[u8]) -> Result<Message, CodecError> {
    Ok(serde_json::from_slice(payload)?)
}

/// Incremental frame reassembler for byte streams delivered in arbitrary chunks.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
}

impl FrameDecoder {
    /// Creates an empty decoder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends received bytes.
    ///
    /// # Errors
    /// Returns [`CodecError::BufferOverflow`] if the buffered bytes would exceed one
    /// maximum-size frame including its header; drain with [`Self::next_frame`] first.
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), CodecError> {
        if self.buffer.len() + bytes.len() > MAX_BUFFER_BYTES {
            return Err(CodecError::BufferOverflow {
                buffered: self.buffer.len(),
                incoming: bytes.len(),
                max: MAX_BUFFER_BYTES,
            });
        }
        self.buffer.extend_from_slice(bytes);
        Ok(())
    }

    /// Returns the next complete frame payload, or `None` if more bytes are needed.
    ///
    /// # Errors
    /// Returns [`CodecError::FrameTooLarge`] as soon as a header announces more
    /// than [`MAX_FRAME_BYTES`], before any payload is awaited.
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, CodecError> {
        let Some(header) = self.buffer.first_chunk::<HEADER_LEN>() else {
            return Ok(None);
        };
        let size = u32::from_be_bytes(*header) as usize;
        if size > MAX_FRAME_BYTES {
            return Err(CodecError::FrameTooLarge {
                size,
                max: MAX_FRAME_BYTES,
            });
        }
        let end = HEADER_LEN + size;
        if self.buffer.len() < end {
            return Ok(None);
        }
        let payload = self.buffer[HEADER_LEN..end].to_vec();
        self.buffer.drain(..end);
        Ok(Some(payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonrpc::{Id, Message, Notification, Version};

    const HEADER_LEN: usize = 4;

    fn payload(len: usize) -> Vec<u8> {
        vec![b'x'; len]
    }

    #[test]
    fn encode_prefixes_big_endian_length() {
        // Act
        let frame = encode_frame(b"abc").unwrap();

        // Assert
        assert_eq!(frame, [0, 0, 0, 3, b'a', b'b', b'c']);
    }

    #[test]
    fn encode_accepts_empty_and_max_payloads() {
        for len in [0, MAX_FRAME_BYTES] {
            // Act
            let frame = encode_frame(&payload(len)).unwrap();

            // Assert
            assert_eq!(frame.len(), HEADER_LEN + len);
        }
    }

    #[test]
    fn encode_rejects_payload_above_max() {
        // Act
        let result = encode_frame(&payload(MAX_FRAME_BYTES + 1));

        // Assert
        assert!(matches!(result, Err(CodecError::FrameTooLarge { .. })));
    }

    #[test]
    fn decoder_yields_empty_frame() {
        // Arrange
        let mut decoder = FrameDecoder::new();
        decoder.push(&[0, 0, 0, 0]).unwrap();

        // Act
        let frame = decoder.next_frame().unwrap();

        // Assert
        assert_eq!(frame, Some(Vec::new()));
    }

    #[test]
    fn decoder_yields_max_frame() {
        // Arrange
        let mut decoder = FrameDecoder::new();
        decoder
            .push(&encode_frame(&payload(MAX_FRAME_BYTES)).unwrap())
            .unwrap();

        // Act
        let frame = decoder.next_frame().unwrap();

        // Assert
        assert_eq!(frame.map(|f| f.len()), Some(MAX_FRAME_BYTES));
    }

    #[test]
    fn decoder_rejects_header_above_max_before_payload_arrives() {
        // Arrange
        let mut decoder = FrameDecoder::new();
        let announced = u32::try_from(MAX_FRAME_BYTES + 1).unwrap();
        decoder.push(&announced.to_be_bytes()).unwrap();

        // Act
        let result = decoder.next_frame();

        // Assert
        assert!(matches!(result, Err(CodecError::FrameTooLarge { .. })));
    }

    #[test]
    fn decoder_handles_one_byte_at_a_time() {
        // Arrange
        let mut decoder = FrameDecoder::new();
        let bytes = encode_frame(b"hello").unwrap();
        let mut frames = Vec::new();

        // Act
        for byte in &bytes {
            decoder.push(std::slice::from_ref(byte)).unwrap();
            if let Some(frame) = decoder.next_frame().unwrap() {
                frames.push(frame);
            }
        }

        // Assert
        assert_eq!(frames, vec![b"hello".to_vec()]);
    }

    #[test]
    fn decoder_splits_two_frames_from_one_push() {
        // Arrange
        let mut decoder = FrameDecoder::new();
        let mut bytes = encode_frame(b"one").unwrap();
        bytes.extend(encode_frame(b"two").unwrap());
        decoder.push(&bytes).unwrap();

        // Act
        let first = decoder.next_frame().unwrap();
        let second = decoder.next_frame().unwrap();
        let third = decoder.next_frame().unwrap();

        // Assert
        assert_eq!(first, Some(b"one".to_vec()));
        assert_eq!(second, Some(b"two".to_vec()));
        assert_eq!(third, None);
    }

    #[test]
    fn message_round_trips_through_frame() {
        // Arrange
        let message = Message::Notification(Notification {
            jsonrpc: Version,
            method: "note".into(),
            params: None,
        });
        let mut decoder = FrameDecoder::new();

        // Act
        decoder.push(&encode_message(&message).unwrap()).unwrap();
        let frame = decoder.next_frame().unwrap().unwrap();
        let parsed = decode_message(&frame).unwrap();

        // Assert
        assert_eq!(parsed, message);
    }

    #[test]
    fn decode_message_rejects_empty_payload() {
        // Act
        let result = decode_message(&[]);

        // Assert
        assert!(matches!(result, Err(CodecError::InvalidJson(_))));
    }

    #[test]
    fn decode_message_reads_golden_request() {
        // Arrange
        let golden = br#"{"jsonrpc":"2.0","id":7,"method":"m"}"#;

        // Act
        let decoded = decode_message(golden).unwrap();

        // Assert
        assert!(matches!(decoded, Message::Request(r) if r.id == Id::Number(7)));
    }

    #[test]
    fn push_beyond_buffer_cap_errors_without_draining() {
        // Arrange
        let mut decoder = FrameDecoder::new();
        let mut header = u32::try_from(MAX_FRAME_BYTES)
            .unwrap()
            .to_be_bytes()
            .to_vec();
        header.extend(payload(MAX_FRAME_BYTES));
        decoder.push(&header).unwrap();

        // Act
        let result = decoder.push(&[0]);

        // Assert
        assert!(matches!(result, Err(CodecError::BufferOverflow { .. })));
    }

    #[test]
    fn draining_a_frame_restores_buffer_capacity() {
        // Arrange
        let mut decoder = FrameDecoder::new();
        decoder
            .push(&encode_frame(&payload(MAX_FRAME_BYTES)).unwrap())
            .unwrap();
        decoder.next_frame().unwrap().unwrap();

        // Act
        let result = decoder.push(&encode_frame(b"ok").unwrap());

        // Assert
        assert!(result.is_ok());
    }
}
