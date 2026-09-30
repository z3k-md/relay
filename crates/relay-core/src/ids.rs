use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::CoreError;

fn parse_hex32(value: &str) -> Result<[u8; 32], CoreError> {
    let mut out = [0u8; 32];
    hex::decode_to_slice(value, &mut out).map_err(|_| CoreError::InvalidId {
        value: value.to_owned(),
        reason: "expected 64 hex characters",
    })?;
    Ok(out)
}

/// Long-lived device identity.
///
/// Phase 2 derives this from the device's Ed25519 public key. Until then it is
/// 32 random bytes generated on first launch; nothing leaves the machine, so
/// the switch does not need a migration of shared state.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DeviceId(#[serde(with = "hex_32")] [u8; 32]);

impl DeviceId {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn random() -> Self {
        Self(rand::random())
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Short, stable label used in human-facing output and conflict names.
    pub fn short(&self) -> String {
        hex::encode(&self.0[..4])
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

impl fmt::Debug for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceId({})", self.short())
    }
}

impl FromStr for DeviceId {
    type Err = CoreError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_hex32(s).map(Self)
    }
}

/// BLAKE3 hash of an object's plaintext bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ObjectId(#[serde(with = "hex_32")] [u8; 32]);

impl ObjectId {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn of(data: &[u8]) -> Self {
        Self(*blake3::hash(data).as_bytes())
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn short(&self) -> String {
        hex::encode(&self.0[..6])
    }
}

impl From<blake3::Hash> for ObjectId {
    fn from(hash: blake3::Hash) -> Self {
        Self(*hash.as_bytes())
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({})", self.short())
    }
}

impl FromStr for ObjectId {
    type Err = CoreError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_hex32(s).map(Self)
    }
}

macro_rules! uuid_id {
    ($name:ident) => {
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            pub fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            pub fn as_uuid(&self) -> &Uuid {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.0)
            }
        }

        impl FromStr for $name {
            type Err = CoreError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(s)
                    .map(Self)
                    .map_err(|_| CoreError::InvalidId {
                        value: s.to_owned(),
                        reason: "expected a UUID",
                    })
            }
        }
    };
}

uuid_id!(SpaceId);
uuid_id!(MountId);
uuid_id!(PolicyId);

/// Position in a device's local change log.
///
/// Every change this device commits to its index (local edit or applied remote
/// update) gets the next sequence number. Peers ask for "everything after N"
/// and acknowledge by watermark instead of per entry.
#[derive(
    Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Sequence(pub u64);

impl Sequence {
    pub const ZERO: Sequence = Sequence(0);

    pub fn next(self) -> Sequence {
        Sequence(self.0 + 1)
    }
}

impl fmt::Display for Sequence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

mod hex_32 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let value = String::deserialize(d)?;
        super::parse_hex32(&value).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_id_matches_blake3_and_round_trips() {
        let id = ObjectId::of(b"hello");
        assert_eq!(id.to_hex(), blake3::hash(b"hello").to_hex().to_string());
        assert_eq!(id.to_hex().parse::<ObjectId>().unwrap(), id);
    }

    #[test]
    fn device_id_round_trips_through_serde() {
        let id = DeviceId::random();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(serde_json::from_str::<DeviceId>(&json).unwrap(), id);
        assert_eq!(id.short().len(), 8);
    }

    #[test]
    fn rejects_malformed_ids() {
        assert!("abc".parse::<ObjectId>().is_err());
        assert!("not-a-uuid".parse::<SpaceId>().is_err());
    }
}
