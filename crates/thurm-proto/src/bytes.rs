//! Serde for byte buffers as one block: `#[serde(with = "bytes")]`.
//!
//! Serde writes a `Vec<u8>` as a sequence, one element at a time, which makes postcard copy
//! PTY output byte by byte. As bytes it is a single copy, and postcard's encoding is the same
//! (a varint length, then the raw bytes), so the wire format doesn't change. JSON still gets an
//! array of numbers.

use std::fmt;

use serde::de::{SeqAccess, Visitor};
use serde::{Deserializer, Serializer};

pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
    s.serialize_bytes(v)
}

pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    struct Bytes;

    impl<'de> Visitor<'de> for Bytes {
        type Value = Vec<u8>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("bytes")
        }

        fn visit_bytes<E>(self, v: &[u8]) -> Result<Vec<u8>, E> {
            Ok(v.to_vec())
        }

        fn visit_byte_buf<E>(self, v: Vec<u8>) -> Result<Vec<u8>, E> {
            Ok(v)
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u8>, A::Error> {
            let mut v = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(1 << 20));
            while let Some(b) = seq.next_element()? {
                v.push(b);
            }
            Ok(v)
        }
    }

    d.deserialize_byte_buf(Bytes)
}

/// The same for an `Option<Vec<u8>>`: `#[serde(with = "bytes::option")]`.
pub mod option {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Serialize)]
    struct BlockRef<'a>(#[serde(serialize_with = "super::serialize")] &'a [u8]);

    #[derive(Deserialize)]
    struct Block(#[serde(deserialize_with = "super::deserialize")] Vec<u8>);

    pub fn serialize<S: Serializer>(v: &Option<Vec<u8>>, s: S) -> Result<S::Ok, S::Error> {
        v.as_deref().map(BlockRef).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
        Ok(Option::<Block>::deserialize(d)?.map(|b| b.0))
    }
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Plain(Vec<u8>);

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Block(#[serde(with = "super")] Vec<u8>);

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Maybe(#[serde(with = "super::option")] Option<Vec<u8>>);

    #[test]
    fn optional_bytes_round_trip() {
        for v in [None, Some(vec![]), Some(vec![0, 7, 255])] {
            let wire = postcard::to_stdvec(&Maybe(v.clone())).unwrap();
            assert_eq!(postcard::from_bytes::<Maybe>(&wire).unwrap().0, v);
            let json = serde_json::to_string(&Maybe(v.clone())).unwrap();
            assert_eq!(serde_json::from_str::<Maybe>(&json).unwrap().0, v);
        }
    }

    #[test]
    fn same_postcard_encoding_and_json_round_trip() {
        let data: Vec<u8> = (0..=255).chain(0..=255).collect();
        let plain = postcard::to_stdvec(&Plain(data.clone())).unwrap();
        let block = postcard::to_stdvec(&Block(data.clone())).unwrap();
        assert_eq!(plain, block, "wire format must not change");
        assert_eq!(postcard::from_bytes::<Block>(&plain).unwrap().0, data);
        let json = serde_json::to_string(&Block(vec![1, 2, 255])).unwrap();
        assert_eq!(json, "[1,2,255]");
        assert_eq!(
            serde_json::from_str::<Block>(&json).unwrap().0,
            vec![1, 2, 255]
        );
    }
}
