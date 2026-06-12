//! Base64 (de)serialization for `Vec<u8>` fields — keeps PTY byte payloads
//! compact on the JSON wire instead of arrays of numbers.

use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Deserializer, Serializer};

pub fn serialize<S: Serializer>(bytes: &[u8], ser: S) -> Result<S::Ok, S::Error> {
    ser.serialize_str(&STANDARD.encode(bytes))
}

pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<u8>, D::Error> {
    let s = String::deserialize(de)?;
    STANDARD.decode(s).map_err(serde::de::Error::custom)
}
