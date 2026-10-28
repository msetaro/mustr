//! Decoding values from a T3 server that is newer than this build.
//!
//! T3 evolves its schemas additively, so a newer server can send a string-enum value, a
//! union tag, or an array element that didn't exist when these types were generated. The
//! generated code handles that in three ways (see `codegen/README.md`):
//!
//! - **String enums** (two or more values) and **tagged unions** have an `Unrecognized`
//!   variant instead of failing. Serializing it sends the original value back.
//! - **T3's own lenient fields** (`ForwardCompatibleArray`, `ForwardCompatibleUnionArray`,
//!   `ForwardCompatibleOptional`/`Nullable`, `OmittedWhenNull`) decode exactly like T3:
//!   their values are decoded strictly, an array element that fails (or, for tagged
//!   unions, has an unknown tag) is dropped, and an optional value that fails becomes
//!   `None`. These fields carry a
//!   `#[serde(deserialize_with = "...")]` pointing at the functions below.
//! - **Untagged unions** (no discriminator) are decoded strictly first, with every
//!   `Unrecognized` fallback disabled, and only leniently if that fails. Otherwise a
//!   fallback in an early variant would capture a value that a later variant matches
//!   exactly.
//! - **Every other list and optional value** of a struct or union type is lenient too, beyond
//!   what T3 itself does ([`lenient_vec`], [`lenient_option_vec`], [`lenient_option`]): a list
//!   element that still doesn't decode is dropped, and an optional value becomes `None`, so
//!   one value this build can't read never fails the snapshot, stream item, or list around it.
//!   Inside a strict pass they decode strictly, so T3's own lenient fields and untagged unions
//!   keep T3's semantics.
//!
//! Each unknown value and each dropped element is logged once per kind ([`note_unrecognized`]).

use std::cell::Cell;
use std::collections::HashSet;
use std::fmt;
use std::sync::{Mutex, OnceLock};

use serde::Deserialize as _;
use serde::de::{DeserializeOwned, Deserializer, Error as _, Visitor};
use serde_json::Value;

/// A generated type that can hold a value this build doesn't know about.
pub trait ForwardCompatible {
    /// True for the `Unrecognized` variant.
    fn is_unrecognized(&self) -> bool;
}

/// A string-enum value this build doesn't know, rejected during a strict pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnrecognizedValue(pub String);

impl fmt::Display for UnrecognizedValue {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        write!(formatter, "unrecognized value {:?}", self.0)
    }
}

impl std::error::Error for UnrecognizedValue {}

thread_local! {
    static STRICT: Cell<bool> = const { Cell::new(false) };
}

/// The kinds of unknown values already logged. Bounded, so a server sending endless distinct
/// values can't grow it.
fn logged_once(key: String) -> bool {
    const MAX_KINDS: usize = 512;
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let mut seen = SEEN
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if seen.len() >= MAX_KINDS || seen.contains(&key) {
        return false;
    }
    seen.insert(key);
    true
}

/// Logs, once per `what` and `value`, that a newer T3 sent a value this build doesn't know and
/// skips (an enum value, union tag, event type, or item kind).
pub fn note_unrecognized(what: &str, value: &str) {
    if logged_once(format!("{what}\0{value}")) {
        log::warn!("T3 sent a {what} this version of mustr doesn't know ({value:?}); skipping it");
    }
}

/// Logs, once per type, that a value of type `T` didn't decode and was dropped.
fn note_dropped<T>(error: &dyn fmt::Display) {
    let name = std::any::type_name::<T>();
    note_skipped(name.rsplit("::").next().unwrap_or(name), error);
}

/// Logs, once per `what`, that a value didn't decode and was left out, such as an item of
/// the stream `what`.
pub fn note_skipped(what: &str, error: &dyn fmt::Display) {
    if logged_once(format!("skipped\0{what}")) {
        log::warn!(
            "T3 sent a {what} this version of mustr can't read, so it was left out: {error}"
        );
    }
}

/// True while decoding strictly (the first pass of an untagged union, or a value inside a
/// lenient field), where `Unrecognized` fallbacks are disabled. T3's open unions
/// (`ForwardCompatibleUnion`) still accept unknown tags, as they do in T3.
pub fn is_strict() -> bool {
    STRICT.with(Cell::get)
}

/// Runs `decode` with every `Unrecognized` fallback disabled.
#[doc(hidden)]
pub fn strictly<T>(decode: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            STRICT.with(|strict| strict.set(self.0));
        }
    }
    let _restore = Restore(STRICT.with(|strict| strict.replace(true)));
    decode()
}

/// Tries `decode` strictly, then leniently. Inside an outer strict pass, only strictly.
#[doc(hidden)]
pub fn strict_first<T, E>(decode: impl Fn() -> Result<T, E>) -> Result<T, E> {
    if is_strict() {
        return decode();
    }
    strictly(&decode).or_else(|_| decode())
}

/// `Deserialize` for a generated untagged union. `derived` is serde's derived
/// implementation (the union carries `#[serde(remote = "Self")]`).
#[doc(hidden)]
pub fn untagged<'de, D, T>(
    deserializer: D,
    derived: impl Fn(&Value) -> Result<T, serde_json::Error>,
) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    strict_first(|| derived(&value)).map_err(D::Error::custom)
}

/// `Deserialize` for a generated string enum named `name`.
#[doc(hidden)]
pub fn string_enum<'de, D, T>(
    deserializer: D,
    name: &'static str,
    known: fn(&str) -> Option<T>,
    unrecognized: fn(String) -> T,
    expected: &'static [&'static str],
) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
{
    struct StringEnumVisitor<T> {
        name: &'static str,
        known: fn(&str) -> Option<T>,
        unrecognized: fn(String) -> T,
        expected: &'static [&'static str],
    }

    impl<T> StringEnumVisitor<T> {
        fn decode<E: serde::de::Error>(&self, value: &str) -> Result<T, E> {
            match (self.known)(value) {
                Some(known) => Ok(known),
                None if is_strict() => Err(E::unknown_variant(value, self.expected)),
                None => {
                    note_unrecognized(self.name, value);
                    Ok((self.unrecognized)(value.to_owned()))
                }
            }
        }
    }

    impl<T> Visitor<'_> for StringEnumVisitor<T> {
        type Value = T;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a string")
        }

        fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<T, E> {
            self.decode(value)
        }

        fn visit_string<E: serde::de::Error>(self, value: String) -> Result<T, E> {
            match (self.known)(&value) {
                Some(known) => Ok(known),
                None if is_strict() => Err(E::unknown_variant(&value, self.expected)),
                None => {
                    note_unrecognized(self.name, &value);
                    Ok((self.unrecognized)(value))
                }
            }
        }
    }

    deserializer.deserialize_str(StringEnumVisitor {
        name,
        known,
        unrecognized,
        expected,
    })
}

/// Decodes one value of a lenient field the way T3 does: with the exact schema, so an
/// unknown enum value or union tag anywhere inside makes it fail (and be dropped).
fn decode_strictly<T: DeserializeOwned>(value: Value) -> Option<T> {
    strictly(|| T::deserialize(value))
        .inspect_err(|error| {
            // Inside an outer strict pass, the failure is the outer value's, not a drop.
            if !is_strict() {
                note_dropped::<T>(error);
            }
        })
        .ok()
}

/// Decodes one list element or optional value leniently, dropping it if it still fails.
fn decode_or_drop<T: DeserializeOwned>(value: Value) -> Option<T> {
    T::deserialize(value)
        .inspect_err(|error| note_dropped::<T>(error))
        .ok()
}

/// `ForwardCompatibleArray` / `ForwardCompatibleUnionArray`: drops elements that don't
/// decode.
pub fn vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    let values = Vec::<Value>::deserialize(deserializer)?;
    Ok(values.into_iter().filter_map(decode_strictly).collect())
}

/// An optional [`vec`].
pub fn option_vec<'de, D, T>(deserializer: D) -> Result<Option<Vec<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    let values = Option::<Vec<Value>>::deserialize(deserializer)?;
    Ok(values.map(|values| values.into_iter().filter_map(decode_strictly).collect()))
}

/// Decodes one element of a lenient array of tagged unions: like [`decode_strictly`], and
/// an `Unrecognized` value (an open union's unknown tag) is dropped too.
fn decode_known<T: DeserializeOwned + ForwardCompatible>(value: Value) -> Option<T> {
    decode_strictly::<T>(value).filter(|element| !element.is_unrecognized())
}

/// [`vec`] for tagged-union elements (`ForwardCompatibleUnionArray`, or a
/// `ForwardCompatibleArray` of a union): also drops elements with an unknown tag.
pub fn known_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned + ForwardCompatible,
{
    let values = Vec::<Value>::deserialize(deserializer)?;
    Ok(values.into_iter().filter_map(decode_known).collect())
}

/// An optional [`known_vec`].
pub fn option_known_vec<'de, D, T>(deserializer: D) -> Result<Option<Vec<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned + ForwardCompatible,
{
    let values = Option::<Vec<Value>>::deserialize(deserializer)?;
    Ok(values.map(|values| values.into_iter().filter_map(decode_known).collect()))
}

/// `ForwardCompatibleOptional` / `ForwardCompatibleNullable` / `OmittedWhenNull`: a value
/// that doesn't decode becomes `None`.
pub fn option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.and_then(decode_strictly))
}

/// A list of structs or unions that T3 decodes strictly: an element that doesn't decode is
/// dropped (and logged), so one value from a newer T3 doesn't fail the whole list. Strict
/// inside a strict pass.
pub fn lenient_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    if is_strict() {
        return Vec::<T>::deserialize(deserializer);
    }
    let values = Vec::<Value>::deserialize(deserializer)?;
    Ok(values.into_iter().filter_map(decode_or_drop).collect())
}

/// An optional [`lenient_vec`].
pub fn lenient_option_vec<'de, D, T>(deserializer: D) -> Result<Option<Vec<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    if is_strict() {
        return Option::<Vec<T>>::deserialize(deserializer);
    }
    let values = Option::<Vec<Value>>::deserialize(deserializer)?;
    Ok(values.map(|values| values.into_iter().filter_map(decode_or_drop).collect()))
}

/// An optional struct or union that T3 decodes strictly: a value that doesn't decode becomes
/// `None` (and is logged). Strict inside a strict pass.
pub fn lenient_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    if is_strict() {
        return Option::<T>::deserialize(deserializer);
    }
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.and_then(decode_or_drop))
}
