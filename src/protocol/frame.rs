use bytes::{BufMut, Bytes, BytesMut};

/// Encode a binary frame with the stream-ID header for sending to the client.
///
/// Wire format:
///   [id_len: 1 byte] [stream_id: id_len bytes UTF-8] [pcm_payload: remaining]
pub fn encode_binary_frame(stream_id: &str, pcm_payload: &[u8]) -> Bytes {
    let id_bytes = stream_id.as_bytes();
    debug_assert!(
        id_bytes.len() <= 255,
        "stream_id too long for single-byte length prefix"
    );

    let mut buf = BytesMut::with_capacity(1 + id_bytes.len() + pcm_payload.len());
    buf.put_u8(id_bytes.len() as u8);
    buf.put_slice(id_bytes);
    buf.put_slice(pcm_payload);
    buf.freeze()
}

/// Decode a binary frame from the client (or for testing).
/// Returns (stream_id, pcm_payload).
pub fn decode_binary_frame(data: &[u8]) -> Result<(&str, &[u8]), FrameError> {
    if data.is_empty() {
        return Err(FrameError::Empty);
    }

    let id_len = data[0] as usize;
    if data.len() < 1 + id_len {
        return Err(FrameError::TooShort {
            need: 1 + id_len,
            got: data.len(),
        });
    }

    let stream_id =
        std::str::from_utf8(&data[1..1 + id_len]).map_err(|_| FrameError::InvalidUtf8)?;
    let pcm = &data[1 + id_len..];

    Ok((stream_id, pcm))
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("empty binary frame")]
    Empty,
    #[error("frame too short: need {need} bytes, got {got}")]
    TooShort { need: usize, got: usize },
    #[error("stream_id is not valid UTF-8")]
    InvalidUtf8,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let pcm = vec![0u8; 1024];
        let encoded = encode_binary_frame("stream-42", &pcm);
        let (sid, payload) = decode_binary_frame(&encoded).unwrap();
        assert_eq!(sid, "stream-42");
        assert_eq!(payload.len(), 1024);
    }

    #[test]
    fn empty_stream_id() {
        let encoded = encode_binary_frame("", &[1, 2, 3]);
        let (sid, payload) = decode_binary_frame(&encoded).unwrap();
        assert_eq!(sid, "");
        assert_eq!(payload, &[1, 2, 3]);
    }

    #[test]
    fn empty_frame_error() {
        let result = decode_binary_frame(&[]);
        assert!(matches!(result, Err(FrameError::Empty)));
    }

    #[test]
    fn truncated_frame_error() {
        // id_len says 10 but only 3 bytes total
        let result = decode_binary_frame(&[10, 0, 0]);
        assert!(matches!(result, Err(FrameError::TooShort { .. })));
    }

    #[test]
    fn empty_payload() {
        let encoded = encode_binary_frame("s1", &[]);
        let (sid, payload) = decode_binary_frame(&encoded).unwrap();
        assert_eq!(sid, "s1");
        assert!(payload.is_empty());
    }
}
