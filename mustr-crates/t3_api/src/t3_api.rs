//! Wire types for T3 Code's WebSocket RPC and HTTP API.
//!
//! `types`, `tagged`, `rpc`, `http` and `named` are generated from T3's Effect Schema
//! contracts by `codegen/regen.sh` (Effect `toJsonSchemaDocument`, then typify; see
//! `codegen/README.md`). Never edit `src/generated/` by hand. Everything in this file and
//! `envelope.rs` is hand-written and depends only on the *Effect* RPC protocol, not on
//! T3's contracts.
//!
//! All types model the **encoded (JSON wire) side** exactly as Effect's
//! `Schema.toCodecJson` produces it, which is what `RpcSerialization.layerJson` sends.
//! Brands are erased (`ThreadId` is a `String`), and `DateTimeUtc` is an ISO string.

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::DeserializeOwned};

#[rustfmt::skip]
#[path = "generated/types.rs"]
pub mod types;

#[rustfmt::skip]
#[path = "generated/tagged.rs"]
pub mod tagged;

#[rustfmt::skip]
#[path = "generated/methods.rs"]
mod methods;
pub use methods::{http, named, rpc};

pub mod envelope;

/// One T3 WebSocket RPC method (`WsRpcGroup` entry).
pub trait RpcMethod {
    /// The `tag` field of the Effect RPC `Request` envelope, e.g. `"pullRequests.list"`.
    const TAG: &'static str;
    /// Stream RPC: the server sends `Chunk` frames of `Success` values, then an `Exit`
    /// whose success value is `null`.
    const STREAM: bool;
    type Payload: Serialize + DeserializeOwned;
    type Success: Serialize + DeserializeOwned;
    /// Union of the RPC's error schema, the stream error and middleware errors
    /// (mirrors `effect/rpc/Rpc.exitSchema`). Appears as `Fail.error` in the `Exit` cause.
    type Error: Serialize + DeserializeOwned;
}

/// One T3 HTTP endpoint (Effect `HttpApi`).
pub trait HttpEndpoint {
    const METHOD: &'static str;
    /// Path template with `:param` placeholders, e.g. `/api/orchestration/threads/:threadId`.
    const PATH: &'static str;
    /// `Some("application/json")` / `Some("application/x-www-form-urlencoded")` / `None`.
    const REQUEST_CONTENT_TYPE: Option<&'static str>;
    const SUCCESS_STATUS: u16;
    const ERROR_STATUSES: &'static [u16];
    type Params;
    type Query;
    type Headers;
    type Request;
    type Response;
}

/// Uninhabited type for RPCs that cannot fail (Effect `Schema.Never`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Never {}

/// Effect `Schema.Number` on the JSON wire: a JSON number, or one of the strings
/// `"Infinity"`, `"-Infinity"`, `"NaN"` (Effect's JSON codec encodes non-finite numbers
/// that way). `Schema.Int` / `Schema.Finite` map to plain `i64` / `f64` instead.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default)]
pub struct JsNumber(pub f64);

impl From<f64> for JsNumber {
    fn from(value: f64) -> Self {
        Self(value)
    }
}

impl From<JsNumber> for f64 {
    fn from(value: JsNumber) -> Self {
        value.0
    }
}

impl Serialize for JsNumber {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let value = self.0;
        if value.is_nan() {
            serializer.serialize_str("NaN")
        } else if value.is_infinite() {
            serializer.serialize_str(if value > 0.0 { "Infinity" } else { "-Infinity" })
        } else if value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_991.0 {
            // Keep integral values integral on the wire (`5`, not `5.0`), like JSON.stringify.
            serializer.serialize_i64(value as i64)
        } else {
            serializer.serialize_f64(value)
        }
    }
}

impl<'de> Deserialize<'de> for JsNumber {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct JsNumberVisitor;

        impl serde::de::Visitor<'_> for JsNumberVisitor {
            type Value = JsNumber;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a number or \"Infinity\" / \"-Infinity\" / \"NaN\"")
            }

            fn visit_f64<E>(self, value: f64) -> Result<JsNumber, E> {
                Ok(JsNumber(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<JsNumber, E> {
                Ok(JsNumber(value as f64))
            }

            fn visit_u64<E>(self, value: u64) -> Result<JsNumber, E> {
                Ok(JsNumber(value as f64))
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<JsNumber, E> {
                match value {
                    "NaN" => Ok(JsNumber(f64::NAN)),
                    "Infinity" => Ok(JsNumber(f64::INFINITY)),
                    "-Infinity" => Ok(JsNumber(f64::NEG_INFINITY)),
                    _ => Err(E::invalid_value(serde::de::Unexpected::Str(value), &self)),
                }
            }
        }

        deserializer.deserialize_any(JsNumberVisitor)
    }
}

/// Discriminated union whose variants are named structs that *keep* their discriminator
/// field (Effect TaggedError / TaggedStruct style). Deserialization dispatches on the tag;
/// an unknown tag (newer T3 server) becomes `Unrecognized(raw)` instead of an error.
/// Serialization writes the inner value as-is (it carries its own tag), so round-trips
/// are lossless.
#[doc(hidden)]
#[macro_export]
macro_rules! tagged_union {
    (
        $(#[$meta:meta])*
        pub enum $name:ident : $tag:literal { $($value:literal => $variant:ident($ty:ty)),* $(,)? }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq)]
        pub enum $name {
            $($variant($ty),)*
            /// A variant this build does not know about (e.g. from a newer T3 server).
            Unrecognized(::serde_json::Value),
        }

        impl $name {
            /// Name of the discriminator field on the wire.
            pub const TAG_FIELD: &'static str = $tag;
            /// Every discriminator value known at generation time.
            pub const KNOWN_TAGS: &'static [&'static str] = &[$($value),*];

            /// The discriminator value of this variant.
            pub fn tag(&self) -> ::std::option::Option<&str> {
                match self {
                    $(Self::$variant(_) => Some($value),)*
                    Self::Unrecognized(value) => value.get($tag).and_then(|tag| tag.as_str()),
                }
            }
        }

        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(&self, serializer: S) -> ::std::result::Result<S::Ok, S::Error> {
                match self {
                    $(Self::$variant(value) => ::serde::Serialize::serialize(value, serializer),)*
                    Self::Unrecognized(value) => ::serde::Serialize::serialize(value, serializer),
                }
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(deserializer: D) -> ::std::result::Result<Self, D::Error> {
                let value = <::serde_json::Value as ::serde::Deserialize>::deserialize(deserializer)?;
                let tag = match value.get($tag) {
                    Some(::serde_json::Value::String(tag)) => tag.clone(),
                    _ => return Ok(Self::Unrecognized(value)),
                };
                // A tag value may map to several variants (tried in order); a known tag whose
                // body fails to decode is an error, an unknown tag is `Unrecognized`.
                let mut error: ::std::option::Option<::std::string::String> = None;
                $(if tag == $value {
                    match <$ty as ::serde::Deserialize>::deserialize(&value) {
                        Ok(variant) => return Ok(Self::$variant(variant)),
                        Err(e) => error = Some(format!("{}::{} ({}={:?}): {}", stringify!($name), stringify!($variant), $tag, $value, e)),
                    }
                })*
                match error {
                    Some(e) => Err(<D::Error as ::serde::de::Error>::custom(e)),
                    None => Ok(Self::Unrecognized(value)),
                }
            }
        }

        $(impl ::std::convert::From<$ty> for $name {
            fn from(value: $ty) -> Self { Self::$variant(value) }
        })*
    };
}
