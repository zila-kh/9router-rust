use crate::error::AppError;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use crc32fast::Hasher;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub name: String,
    pub value: HeaderValue,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderValue {
    Bool(bool),
    Byte(i8),
    Int16(i16),
    Int32(i32),
    Int64(i64),
    Bytes(Vec<u8>),
    String(String),
    Timestamp(i64),
    Uuid([u8; 16]),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub headers: Vec<Header>,
    pub payload: Bytes,
}

pub fn encode(msg: &Message) -> Result<Bytes, AppError> {
    let mut hb = BytesMut::new();
    for h in &msg.headers {
        if h.name.len() > 255 {
            return Err(AppError::BadRequest(
                "EventStream header name too long".into(),
            ));
        }
        hb.put_u8(h.name.len() as u8);
        hb.extend_from_slice(h.name.as_bytes());
        match &h.value {
            HeaderValue::Bool(false) => hb.put_u8(0),
            HeaderValue::Bool(true) => hb.put_u8(1),
            HeaderValue::Byte(v) => {
                hb.put_u8(2);
                hb.put_i8(*v)
            }
            HeaderValue::Int16(v) => {
                hb.put_u8(3);
                hb.put_i16(*v)
            }
            HeaderValue::Int32(v) => {
                hb.put_u8(4);
                hb.put_i32(*v)
            }
            HeaderValue::Int64(v) => {
                hb.put_u8(5);
                hb.put_i64(*v)
            }
            HeaderValue::Bytes(v) => {
                hb.put_u8(6);
                hb.put_u16(v.len() as u16);
                hb.extend_from_slice(v)
            }
            HeaderValue::String(v) => {
                hb.put_u8(7);
                hb.put_u16(v.len() as u16);
                hb.extend_from_slice(v.as_bytes())
            }
            HeaderValue::Timestamp(v) => {
                hb.put_u8(8);
                hb.put_i64(*v)
            }
            HeaderValue::Uuid(v) => {
                hb.put_u8(9);
                hb.extend_from_slice(v)
            }
        }
    }
    let total = 16 + hb.len() + msg.payload.len();
    let mut out = BytesMut::with_capacity(total);
    out.put_u32(total as u32);
    out.put_u32(hb.len() as u32);
    let mut pre = Hasher::new();
    pre.update(&out[..8]);
    let pre_crc = pre.finalize();
    out.put_u32(pre_crc);
    out.extend_from_slice(&hb);
    out.extend_from_slice(&msg.payload);
    let mut h = Hasher::new();
    h.update(&out);
    out.put_u32(h.finalize());
    Ok(out.freeze())
}

pub fn decode_one(buf: &mut BytesMut) -> Result<Option<Message>, AppError> {
    if buf.len() < 12 {
        return Ok(None);
    }
    let total = u32::from_be_bytes(buf[0..4].try_into().unwrap()) as usize;
    let headers_len = u32::from_be_bytes(buf[4..8].try_into().unwrap()) as usize;
    if total < 16 || headers_len > total - 16 {
        return Err(AppError::BadRequest(
            "invalid EventStream frame lengths".into(),
        ));
    }
    if buf.len() < total {
        return Ok(None);
    }
    let expected_pre = u32::from_be_bytes(buf[8..12].try_into().unwrap());
    let mut ph = Hasher::new();
    ph.update(&buf[..8]);
    if ph.finalize() != expected_pre {
        return Err(AppError::BadRequest(
            "EventStream prelude CRC mismatch".into(),
        ));
    }
    let expected = u32::from_be_bytes(buf[total - 4..total].try_into().unwrap());
    let mut fh = Hasher::new();
    fh.update(&buf[..total - 4]);
    if fh.finalize() != expected {
        return Err(AppError::BadRequest(
            "EventStream message CRC mismatch".into(),
        ));
    }
    let frame = buf.split_to(total).freeze();
    let mut hbuf = &frame[12..12 + headers_len];
    let mut headers = Vec::new();
    while hbuf.has_remaining() {
        if hbuf.remaining() < 2 {
            return Err(AppError::BadRequest("truncated EventStream header".into()));
        }
        let n = hbuf.get_u8() as usize;
        if hbuf.remaining() < n + 1 {
            return Err(AppError::BadRequest("truncated EventStream header".into()));
        }
        let name = String::from_utf8(hbuf.copy_to_bytes(n).to_vec())
            .map_err(|_| AppError::BadRequest("invalid header utf8".into()))?;
        let ty = hbuf.get_u8();
        let val = match ty {
            0 => HeaderValue::Bool(false),
            1 => HeaderValue::Bool(true),
            2 => HeaderValue::Byte(hbuf.get_i8()),
            3 => HeaderValue::Int16(hbuf.get_i16()),
            4 => HeaderValue::Int32(hbuf.get_i32()),
            5 => HeaderValue::Int64(hbuf.get_i64()),
            6 => {
                let n = hbuf.get_u16() as usize;
                HeaderValue::Bytes(hbuf.copy_to_bytes(n).to_vec())
            }
            7 => {
                let n = hbuf.get_u16() as usize;
                HeaderValue::String(
                    String::from_utf8(hbuf.copy_to_bytes(n).to_vec())
                        .map_err(|_| AppError::BadRequest("invalid header utf8".into()))?,
                )
            }
            8 => HeaderValue::Timestamp(hbuf.get_i64()),
            9 => {
                let mut u = [0; 16];
                hbuf.copy_to_slice(&mut u);
                HeaderValue::Uuid(u)
            }
            _ => {
                return Err(AppError::BadRequest(
                    "unknown EventStream header type".into(),
                ))
            }
        };
        headers.push(Header { name, value: val });
    }
    let payload = frame.slice(12 + headers_len..total - 4);
    Ok(Some(Message { headers, payload }))
}

impl Message {
    pub fn header_str(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|h| h.name == name).and_then(|h| {
            if let HeaderValue::String(v) = &h.value {
                Some(v.as_str())
            } else {
                None
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn round_trip_eventstream() {
        let msg = Message {
            headers: vec![
                Header {
                    name: ":message-type".into(),
                    value: HeaderValue::String("event".into()),
                },
                Header {
                    name: ":event-type".into(),
                    value: HeaderValue::String("assistantResponseEvent".into()),
                },
            ],
            payload: Bytes::from_static(br#"{"content":"hello"}"#),
        };
        let encoded = encode(&msg).unwrap();
        let mut buf = BytesMut::from(encoded.as_ref());
        let decoded = decode_one(&mut buf).unwrap().unwrap();
        assert_eq!(decoded, msg);
        assert!(buf.is_empty());
    }
    #[test]
    fn rejects_crc_corruption() {
        let msg = Message {
            headers: vec![],
            payload: Bytes::from_static(b"abc"),
        };
        let mut encoded = encode(&msg).unwrap().to_vec();
        encoded[12] ^= 1;
        let mut buf = BytesMut::from(encoded.as_slice());
        assert!(decode_one(&mut buf).is_err());
    }
}
