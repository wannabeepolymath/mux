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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(data: &[u8]) -> (&str, &[u8]) {
        let id_len = data[0] as usize;
        let sid = std::str::from_utf8(&data[1..1 + id_len]).unwrap();
        let pcm = &data[1 + id_len..];
        (sid, pcm)
    }

    #[test]
    fn roundtrip() {
        let pcm = vec![0u8; 1024];
        let encoded = encode_binary_frame("stream-42", &pcm);
        let (sid, payload) = parse(&encoded);
        assert_eq!(sid, "stream-42");
        assert_eq!(payload.len(), 1024);
    }

    #[test]
    fn empty_stream_id() {
        let encoded = encode_binary_frame("", &[1, 2, 3]);
        let (sid, payload) = parse(&encoded);
        assert_eq!(sid, "");
        assert_eq!(payload, &[1, 2, 3]);
    }

    #[test]
    fn empty_payload() {
        let encoded = encode_binary_frame("s1", &[]);
        let (sid, payload) = parse(&encoded);
        assert_eq!(sid, "s1");
        assert!(payload.is_empty());
    }

    #[test]
    fn header_layout() {
        let encoded = encode_binary_frame("ab", &[0xff, 0xee]);
        // 1 byte id_len, 2 bytes id, 2 bytes payload
        assert_eq!(encoded.len(), 5);
        assert_eq!(encoded[0], 2);
        assert_eq!(&encoded[1..3], b"ab");
        assert_eq!(&encoded[3..5], &[0xff, 0xee]);
    }
}
