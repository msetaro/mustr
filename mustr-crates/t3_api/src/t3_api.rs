//! Wire types for T3 Code's WebSocket RPC and HTTP API.
//!
//! `types`, `tagged`, `rpc`, `http` and `named` are generated from T3's Effect Schema
//! contracts by `codegen/regen.sh` (Effect `toJsonSchemaDocument`, then typify plus a
//! post-processing pass; see `codegen/README.md`). Never edit `src/generated/` by hand.
//! Everything in this file, `envelope.rs` and `forward_compat.rs` is hand-written and
//! depends only on the *Effect* RPC protocol, not on T3's contracts.
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
pub mod forward_compat;

pub use forward_compat::ForwardCompatible;

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
    /// Required headers with a fixed value, which a client sends as-is, e.g.
    /// `("x-t3-orchestration-protocol", "2")` on the orchestration snapshot endpoints.
    const CONSTANT_HEADERS: &'static [(&'static str, &'static str)];
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

/// A string-literal enum (`Schema.Literals`) with a fallback for values this build
/// doesn't know. Generated code invokes this instead of deriving serde, so a newer T3
/// adding a value doesn't break decoding. Single-value enums (constants such as `_tag`)
/// stay strict and are emitted by typify directly.
#[doc(hidden)]
#[macro_export]
macro_rules! string_enum {
    (
        $(#[$meta:meta])*
        pub enum $name:ident { $($(#[$variant_meta:meta])* $value:literal => $variant:ident),+ $(,)? }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub enum $name {
            $($(#[$variant_meta])* $variant,)+
            /// A value this build doesn't know (e.g. from a newer T3 server). It is sent
            /// back verbatim.
            Unrecognized(::std::string::String),
        }

        impl $name {
            /// Every value known at generation time.
            pub const KNOWN_VALUES: &'static [&'static str] = &[$($value),+];

            /// The value as it appears on the wire.
            pub fn as_str(&self) -> &str {
                match self {
                    $(Self::$variant => $value,)+
                    Self::Unrecognized(value) => value,
                }
            }

            fn from_known(value: &str) -> ::std::option::Option<Self> {
                match value {
                    $($value => ::std::option::Option::Some(Self::$variant),)+
                    _ => ::std::option::Option::None,
                }
            }
        }

        impl $crate::forward_compat::ForwardCompatible for $name {
            fn is_unrecognized(&self) -> bool {
                matches!(self, Self::Unrecognized(_))
            }
        }

        impl ::std::convert::From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::from_known(value).unwrap_or_else(|| Self::Unrecognized(value.to_owned()))
            }
        }

        impl ::std::convert::From<::std::string::String> for $name {
            fn from(value: ::std::string::String) -> Self {
                Self::from_known(&value).unwrap_or(Self::Unrecognized(value))
            }
        }

        /// Unknown values parse as `Unrecognized`, except inside the strict pass of an
        /// untagged union (see `forward_compat`).
        impl ::std::str::FromStr for $name {
            type Err = $crate::forward_compat::UnrecognizedValue;

            fn from_str(value: &str) -> ::std::result::Result<Self, Self::Err> {
                match Self::from_known(value) {
                    ::std::option::Option::Some(known) => Ok(known),
                    ::std::option::Option::None if $crate::forward_compat::is_strict() => {
                        Err($crate::forward_compat::UnrecognizedValue(value.to_owned()))
                    }
                    ::std::option::Option::None => Ok(Self::Unrecognized(value.to_owned())),
                }
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(&self, serializer: S) -> ::std::result::Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(deserializer: D) -> ::std::result::Result<Self, D::Error> {
                $crate::forward_compat::string_enum(
                    deserializer,
                    stringify!($name),
                    Self::from_known,
                    Self::Unrecognized,
                    Self::KNOWN_VALUES,
                )
            }
        }
    };
}

/// Discriminated union whose variants are named structs that *keep* their discriminator
/// field (Effect TaggedError / TaggedStruct style). Deserialization dispatches on the tag;
/// an unknown tag (newer T3 server) becomes `Unrecognized(raw)` instead of an error,
/// except while decoding strictly (see `forward_compat`) unless the union is `open`
/// (T3's `ForwardCompatibleUnion`, which has its own catch-all member). Serialization
/// writes the inner value as-is (it carries its own tag), so round-trips are lossless.
#[doc(hidden)]
#[macro_export]
macro_rules! tagged_union {
    (
        $(#[$meta:meta])*
        pub enum $name:ident : $tag:literal, open = $open:literal { $($value:literal => $variant:ident($ty:ty)),* $(,)? }
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

        impl $crate::forward_compat::ForwardCompatible for $name {
            fn is_unrecognized(&self) -> bool {
                matches!(self, Self::Unrecognized(_))
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
                use ::serde::de::Error as _;
                let value = <::serde_json::Value as ::serde::Deserialize>::deserialize(deserializer)?;
                let tag = match value.get($tag) {
                    Some(::serde_json::Value::String(tag)) if Self::KNOWN_TAGS.contains(&tag.as_str()) => tag.clone(),
                    // T3's `ForwardCompatibleUnion` (`open`) accepts unknown tags itself.
                    _ if $crate::forward_compat::is_strict() && !$open => {
                        return Err(D::Error::custom(format!(
                            "{}: no known {:?} in {}",
                            stringify!($name),
                            $tag,
                            value
                        )));
                    }
                    _ => {
                        let tag = match value.get($tag) {
                            Some(::serde_json::Value::String(tag)) => tag.as_str(),
                            _ => "",
                        };
                        $crate::forward_compat::note_unrecognized(stringify!($name), tag);
                        return Ok(Self::Unrecognized(value));
                    }
                };
                // A tag value may map to several variants, tried in order; a known tag whose
                // body fails to decode is an error. With several candidates, all are tried
                // strictly first, so a lenient early candidate can't capture a later one's
                // value.
                let candidates = Self::KNOWN_TAGS.iter().filter(|known| **known == tag).count();
                let decode = || -> ::std::result::Result<Self, ::std::string::String> {
                    let mut error = ::std::string::String::new();
                    $(if tag == $value {
                        match <$ty as ::serde::Deserialize>::deserialize(&value) {
                            Ok(variant) => return Ok(Self::$variant(variant)),
                            Err(e) => error = format!("{}::{} ({}={:?}): {}", stringify!($name), stringify!($variant), $tag, $value, e),
                        }
                    })*
                    Err(error)
                };
                let result = if candidates > 1 {
                    $crate::forward_compat::strict_first(decode)
                } else {
                    decode()
                };
                result.map_err(D::Error::custom)
            }
        }

        $(impl ::std::convert::From<$ty> for $name {
            fn from(value: $ty) -> Self { Self::$variant(value) }
        })*
    };
}
