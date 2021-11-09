use core::ops::Deref;

#[cfg(feature = "borsh")]
use borsh::maybestd::io::{Error as BorshError, ErrorKind, Write};
#[cfg(feature = "borsh")]
use borsh::{BorshDeserialize, BorshSerialize};
use prost_types::Any;
use serde_derive::{Deserialize, Serialize};
use subtle_encoding::hex;
use tendermint_proto::Protobuf;

use crate::clients::ics07_tendermint::header::{decode_header, Header as TendermintHeader};
use crate::core::ics02_client::client_type::ClientType;
use crate::core::ics02_client::error::Error;
#[cfg(any(test, feature = "mocks"))]
use crate::mock::header::MockHeader;
use crate::prelude::*;
use crate::timestamp::Timestamp;
use crate::Height;

pub const TENDERMINT_HEADER_TYPE_URL: &str = "/ibc.lightclients.tendermint.v1.Header";
pub const MOCK_HEADER_TYPE_URL: &str = "/ibc.mock.Header";

/// Abstract of consensus state update information
pub trait Header: Clone + core::fmt::Debug + Send + Sync {
    /// The type of client (eg. Tendermint)
    fn client_type(&self) -> ClientType;

    /// The height of the consensus state
    fn height(&self) -> Height;

    /// The timestamp of the consensus state
    fn timestamp(&self) -> Timestamp;

    /// Wrap into an `AnyHeader`
    fn wrap_any(self) -> AnyHeader;
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[allow(clippy::large_enum_variant)]
pub enum AnyHeader {
    Tendermint(TendermintHeader),

    #[cfg(any(test, feature = "mocks"))]
    Mock(MockHeader),
}

impl Header for AnyHeader {
    fn client_type(&self) -> ClientType {
        match self {
            Self::Tendermint(header) => header.client_type(),

            #[cfg(any(test, feature = "mocks"))]
            Self::Mock(header) => header.client_type(),
        }
    }

    fn height(&self) -> Height {
        match self {
            Self::Tendermint(header) => header.height(),

            #[cfg(any(test, feature = "mocks"))]
            Self::Mock(header) => header.height(),
        }
    }

    fn timestamp(&self) -> Timestamp {
        match self {
            Self::Tendermint(header) => header.timestamp(),
            #[cfg(any(test, feature = "mocks"))]
            Self::Mock(header) => header.timestamp(),
        }
    }

    fn wrap_any(self) -> AnyHeader {
        self
    }
}

impl AnyHeader {
    pub fn encode_to_string(&self) -> String {
        let buf = Protobuf::encode_vec(self).expect("encoding shouldn't fail");
        let encoded = hex::encode(buf);
        String::from_utf8(encoded).expect("hex-encoded string should always be valid UTF-8")
    }

    pub fn decode_from_string(s: &str) -> Result<Self, Error> {
        let header_bytes = hex::decode(s).unwrap();
        Protobuf::decode(header_bytes.as_ref()).map_err(Error::invalid_raw_header)
    }
}

#[cfg(feature = "borsh")]
impl BorshSerialize for AnyHeader {
    fn serialize<W: Write>(&self, writer: &mut W) -> Result<(), BorshError> {
        let vec = self
            .encode_vec()
            .expect("AnyHeader encoding shouldn't fail");
        let bytes = vec
            .try_to_vec()
            .expect("AnyHeader bytes encoding shouldn't fail");
        writer.write_all(&bytes)
    }
}

#[cfg(feature = "borsh")]
impl BorshDeserialize for AnyHeader {
    fn deserialize(buf: &mut &[u8]) -> Result<Self, BorshError> {
        let vec: Vec<u8> = BorshDeserialize::deserialize(buf).map_err(|e| {
            BorshError::new(
                ErrorKind::InvalidInput,
                format!("Error decoding AnyHeader from bytes: {}", e),
            )
        })?;
        AnyHeader::decode_vec(&vec).map_err(|e| {
            BorshError::new(
                ErrorKind::InvalidInput,
                format!("Error decoding AnyHeader from Vec<u8>: {}", e),
            )
        })
    }
}

impl Protobuf<Any> for AnyHeader {}

impl TryFrom<Any> for AnyHeader {
    type Error = Error;

    fn try_from(raw: Any) -> Result<Self, Error> {
        match raw.type_url.as_str() {
            TENDERMINT_HEADER_TYPE_URL => {
                let val = decode_header(raw.value.deref()).map_err(Error::tendermint)?;

                Ok(AnyHeader::Tendermint(val))
            }

            #[cfg(any(test, feature = "mocks"))]
            MOCK_HEADER_TYPE_URL => Ok(AnyHeader::Mock(
                MockHeader::decode_vec(&raw.value).map_err(Error::invalid_raw_header)?,
            )),

            _ => Err(Error::unknown_header_type(raw.type_url)),
        }
    }
}

impl From<AnyHeader> for Any {
    fn from(value: AnyHeader) -> Self {
        match value {
            AnyHeader::Tendermint(header) => Any {
                type_url: TENDERMINT_HEADER_TYPE_URL.to_string(),
                value: header
                    .encode_vec()
                    .expect("encoding to `Any` from `AnyHeader::Tendermint`"),
            },
            #[cfg(any(test, feature = "mocks"))]
            AnyHeader::Mock(header) => Any {
                type_url: MOCK_HEADER_TYPE_URL.to_string(),
                value: header
                    .encode_vec()
                    .expect("encoding to `Any` from `AnyHeader::Mock`"),
            },
        }
    }
}
