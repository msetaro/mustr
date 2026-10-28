//! Forward-compatibility pass over typify's output (`types.rs`).
//!
//! typify has no unknown-value fallback for string enums and no per-field attributes, so
//! we rewrite its token stream with syn:
//!
//! 1. String enums with two or more values become `crate::string_enum!` invocations
//!    (an `Unrecognized(String)` variant plus hand-written serde impls); typify's
//!    `Display` / `FromStr` / `TryFrom` impls for them are dropped (the macro provides
//!    infallible ones). Single-value enums (constants such as `_tag`) stay strict.
//! 2. `Copy` is removed from every type that now contains a non-`Copy` enum, and
//!    `match *self` in their impls becomes `match self`.
//! 3. Untagged unions get `#[serde(remote = "Self")]` and a `Deserialize` that runs
//!    serde's derived code strictly first (`crate::forward_compat::untagged`).
//! 4. Fields listed in the manifest's `forwardCompat` (T3's lenient fields) get a
//!    `#[serde(deserialize_with = "crate::forward_compat::...")]` decoder.
//! 5. Every other list or optional field of a struct or union type gets a lenient decoder
//!    (`lenient_vec`, `lenient_option_vec`, `lenient_option`), which drops an element or value
//!    that doesn't decode instead of failing the whole value around it.
//!
//! It also refuses serde-tagged enums (typify's own discriminated unions), which would
//! have no fallback: dump-schemas.ts must hoist those into `tagged.rs`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use anyhow::{Context as _, Result, bail};
use proc_macro2::{TokenStream, TokenTree};
use quote::{ToTokens, quote};
use syn::punctuated::Punctuated;
use syn::visit_mut::VisitMut;
use syn::{Attribute, Fields, Item, ItemEnum, LitStr, Path, Token, Type, parse_quote};

/// One field T3 decodes leniently (from the manifest).
pub struct LenientField {
    /// Rust ident of the owning struct.
    pub owner: String,
    /// JSON property name.
    pub property: String,
    /// `"array"` or `"option"`.
    pub kind: String,
}

pub struct Output {
    /// The rewritten `types.rs` items, ready for prettyplease.
    pub file: syn::File,
    /// `crate::string_enum!` invocations, formatted by hand (prettyplease flows macro
    /// bodies into one paragraph).
    pub string_enums: String,
    pub stats: BTreeMap<&'static str, usize>,
}

pub fn run(tokens: TokenStream, lenient_fields: &[LenientField]) -> Result<Output> {
    let mut file: syn::File = syn::parse2(tokens).context("parse typify output")?;
    let mut stats = BTreeMap::new();

    // 0. serde-tagged enums have no `Unrecognized` fallback; dump-schemas.ts hoists every
    //    discriminated union into `tagged.rs` instead. One showing up here is a new
    //    schema shape the hoisting pass doesn't know yet.
    for item in &file.items {
        if let Item::Enum(e) = item
            && serde_flags(&e.attrs)?.contains("tag")
        {
            bail!(
                "typify emitted the serde-tagged enum {} (no fallback for unknown tags); teach dump-schemas.ts to hoist it into a tagged union",
                e.ident
            );
        }
    }

    // 1. String enums.
    let mut enums: BTreeMap<String, Vec<EnumValue>> = BTreeMap::new();
    let mut enum_docs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut order = Vec::new();
    for item in &file.items {
        if let Item::Enum(e) = item
            && let Some(values) = string_enum_values(e)?
            && values.len() >= 2
        {
            let name = e.ident.to_string();
            order.push(name.clone());
            enum_docs.insert(name.clone(), doc_lines(&e.attrs));
            enums.insert(name, values);
        }
    }
    let replaced_traits = ["Display", "FromStr", "TryFrom"];
    file.items.retain(|item| match item {
        Item::Enum(e) => !enums.contains_key(&e.ident.to_string()),
        Item::Impl(i) => {
            let is_enum = type_ident(&i.self_ty).is_some_and(|n| enums.contains_key(&n));
            let replaced = i.trait_.as_ref().is_some_and(|(path, _)| {
                path.segments
                    .last()
                    .is_some_and(|s| replaced_traits.contains(&s.ident.to_string().as_str()))
            });
            !(is_enum && replaced)
        }
        _ => true,
    });
    stats.insert("stringEnumsWithFallback", enums.len());

    let mut string_enums = String::new();
    for name in &order {
        writeln!(string_enums, "crate::string_enum! {{")?;
        for line in &enum_docs[name] {
            writeln!(string_enums, "    {line}")?;
        }
        writeln!(string_enums, "    pub enum {name} {{")?;
        for v in &enums[name] {
            for line in &v.docs {
                writeln!(string_enums, "        {line}")?;
            }
            writeln!(string_enums, "        {:?} => {},", v.wire, v.ident)?;
        }
        writeln!(string_enums, "    }}\n}}")?;
    }

    // 2. Copy propagation.
    let mut not_copy: BTreeSet<String> = enums.keys().cloned().collect();
    let mut decopied = BTreeSet::new();
    loop {
        let mut changed = false;
        for item in &mut file.items {
            let (attrs, ident, mentioned) = match item {
                Item::Enum(e) => (&mut e.attrs, e.ident.to_string(), idents_in(&e.variants)),
                Item::Struct(s) => (&mut s.attrs, s.ident.to_string(), idents_in(&s.fields)),
                _ => continue,
            };
            if !mentioned.iter().any(|m| not_copy.contains(m)) {
                continue;
            }
            if remove_derive(attrs, "Copy")? {
                not_copy.insert(ident.clone());
                decopied.insert(ident);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    for item in &mut file.items {
        if let Item::Impl(i) = item
            && type_ident(&i.self_ty).is_some_and(|n| decopied.contains(&n))
        {
            MatchSelfByRef.visit_item_impl_mut(i);
        }
    }
    stats.insert("typesNoLongerCopy", decopied.len());

    // 3. Untagged unions: strict pass first.
    let mut items = Vec::with_capacity(file.items.len());
    let mut untagged = 0;
    for mut item in std::mem::take(&mut file.items) {
        let mut extra: Vec<Item> = Vec::new();
        if let Item::Enum(e) = &mut item
            && serde_flags(&e.attrs)?.contains("untagged")
            && has_derive(&e.attrs, "Deserialize")?
        {
            if !has_derive(&e.attrs, "Serialize")? {
                bail!("untagged enum {} has no Serialize derive", e.ident);
            }
            e.attrs.push(parse_quote!(#[serde(remote = "Self")]));
            let name = &e.ident;
            extra.push(parse_quote! {
                impl ::serde::Serialize for #name {
                    fn serialize<S: ::serde::Serializer>(&self, serializer: S) -> ::std::result::Result<S::Ok, S::Error> {
                        #name::serialize(self, serializer)
                    }
                }
            });
            extra.push(parse_quote! {
                impl<'de> ::serde::Deserialize<'de> for #name {
                    fn deserialize<D: ::serde::Deserializer<'de>>(deserializer: D) -> ::std::result::Result<Self, D::Error> {
                        crate::forward_compat::untagged(deserializer, |value| #name::deserialize(value))
                    }
                }
            });
            untagged += 1;
        }
        items.push(item);
        items.extend(extra);
    }
    file.items = items;
    stats.insert("untaggedUnionsStrictFirst", untagged);
    // ...and so does `FromStr` on the string-like ones (the string enums' `FromStr` only
    // fails during a strict pass).
    let untagged_names: BTreeSet<String> = file
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Enum(e) if serde_flags(&e.attrs).is_ok_and(|f| f.contains("untagged")) => {
                Some(e.ident.to_string())
            }
            _ => None,
        })
        .collect();
    for item in &mut file.items {
        let Item::Impl(i) = item else { continue };
        let is_from_str = i
            .trait_
            .as_ref()
            .is_some_and(|(path, _)| path_is(path, "FromStr"));
        if !is_from_str || !type_ident(&i.self_ty).is_some_and(|n| untagged_names.contains(&n)) {
            continue;
        }
        for impl_item in &mut i.items {
            if let syn::ImplItem::Fn(f) = impl_item
                && f.sig.ident == "from_str"
            {
                let body = &f.block;
                f.block = parse_quote!({ crate::forward_compat::strict_first(|| #body) });
            }
        }
    }

    // 4. Lenient fields.
    let mut applied = 0;
    for field in lenient_fields {
        let owner = file
            .items
            .iter_mut()
            .find_map(|item| match item {
                Item::Struct(s) if s.ident == field.owner => Some(s),
                _ => None,
            })
            .with_context(|| {
                format!(
                    "forward-compat: no struct {} for {}",
                    field.owner, field.property
                )
            })?;
        let Fields::Named(named) = &mut owner.fields else {
            bail!(
                "forward-compat: {} is not a struct with named fields",
                field.owner
            );
        };
        let mut found = None;
        for f in named.named.iter_mut() {
            let json = serde_rename(&f.attrs)?.unwrap_or_else(|| {
                f.ident
                    .as_ref()
                    .map(|i| i.to_string().trim_start_matches("r#").to_string())
                    .unwrap_or_default()
            });
            if json == field.property {
                found = Some(f);
                break;
            }
        }
        let f = found.with_context(|| {
            format!(
                "forward-compat: {} has no field {:?}",
                field.owner, field.property
            )
        })?;
        let decoder = decoder_for(&f.ty, &field.kind).with_context(|| {
            format!(
                "forward-compat: {}.{} ({}) has unexpected type {}",
                field.owner,
                field.property,
                field.kind,
                f.ty.to_token_stream()
            )
        })?;
        let path = format!("crate::forward_compat::{decoder}");
        if serde_flags(&f.attrs)?.contains("default") {
            f.attrs
                .push(parse_quote!(#[serde(deserialize_with = #path)]));
        } else {
            f.attrs
                .push(parse_quote!(#[serde(default, deserialize_with = #path)]));
        }
        applied += 1;
    }
    stats.insert("lenientFields", applied);

    // 5. Lenient lists and optionals everywhere else.
    let mut containers = 0;
    for item in &mut file.items {
        let fields: Vec<&mut syn::Field> = match item {
            Item::Struct(s) => match &mut s.fields {
                Fields::Named(named) => named.named.iter_mut().collect(),
                // `#[serde(transparent)] struct Chunk(Vec<T>)`, a list on its own.
                Fields::Unnamed(unnamed) if unnamed.unnamed.len() == 1 => {
                    unnamed.unnamed.iter_mut().collect()
                }
                _ => continue,
            },
            Item::Enum(e) => e
                .variants
                .iter_mut()
                .flat_map(|v| match &mut v.fields {
                    Fields::Named(named) => named.named.iter_mut().collect::<Vec<_>>(),
                    _ => Vec::new(),
                })
                .collect(),
            _ => continue,
        };
        for f in fields {
            if make_lenient(f, &enums)? {
                containers += 1;
            }
        }
    }
    stats.insert("lenientContainers", containers);

    Ok(Output {
        file,
        string_enums,
        stats,
    })
}

/// Types whose values can't fail to decode leniently except on a JSON type mismatch: scalars,
/// untyped JSON, and string enums (which have `Unrecognized`).
fn is_plain(ty: &Type, string_enums: &BTreeMap<String, Vec<EnumValue>>) -> bool {
    const PLAIN: &[&str] = &[
        "String", "bool", "i64", "u64", "i32", "u32", "f64", "f32", "Value", "JsNumber",
    ];
    type_ident(ty)
        .is_some_and(|name| PLAIN.contains(&name.as_str()) || string_enums.contains_key(&name))
        && generic_arg(ty, "Box").is_none()
}

/// Gives a list or optional field of a struct or union type a lenient decoder (step 5).
/// Leaves fields that already have a decoder of their own (T3's lenient fields) alone, and keeps
/// whether the key is required: a required nullable field's `Option::deserialize` is replaced,
/// and an optional key gets `default`.
fn make_lenient(
    field: &mut syn::Field,
    string_enums: &BTreeMap<String, Vec<EnumValue>>,
) -> Result<bool> {
    const REQUIRED_NULLABLE: &str = "::std::option::Option::deserialize";
    let decoder = serde_value(&field.attrs, "deserialize_with")?;
    let flags = serde_flags(&field.attrs)?;
    if flags.contains("with") || decoder.as_deref().is_some_and(|d| d != REQUIRED_NULLABLE) {
        return Ok(false);
    }
    let lenient = if let Some(element) = generic_arg(&field.ty, "Vec") {
        (!is_plain(element, string_enums)).then_some("lenient_vec")
    } else if let Some(inner) = generic_arg(&field.ty, "Option") {
        match generic_arg(inner, "Vec") {
            Some(element) => (!is_plain(element, string_enums)).then_some("lenient_option_vec"),
            None => (!is_plain(inner, string_enums)).then_some("lenient_option"),
        }
    } else {
        None
    };
    let Some(lenient) = lenient else {
        return Ok(false);
    };
    let path = format!("crate::forward_compat::{lenient}");
    if decoder.is_some() {
        // `deserialize_with = "::std::option::Option::deserialize"`: the key stays required.
        for attr in field
            .attrs
            .iter_mut()
            .filter(|a| a.path().is_ident("serde"))
        {
            let text = attr
                .to_token_stream()
                .to_string()
                .replace(&format!("{REQUIRED_NULLABLE:?}"), &format!("{path:?}"));
            *attr = syn::parse_str::<syn::DeriveInput>(&format!("{text} struct X;"))?
                .attrs
                .remove(0);
        }
    } else if generic_arg(&field.ty, "Option").is_some() && !flags.contains("default") {
        field
            .attrs
            .push(parse_quote!(#[serde(default, deserialize_with = #path)]));
    } else {
        field
            .attrs
            .push(parse_quote!(#[serde(deserialize_with = #path)]));
    }
    Ok(true)
}

/// The string value of `key = "..."` inside the field's `#[serde(...)]` attributes.
fn serde_value(attrs: &[Attribute], key: &str) -> Result<Option<String>> {
    let mut out = None;
    for attr in attrs.iter().filter(|a| a.path().is_ident("serde")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident(key) {
                let value: LitStr = meta.value()?.parse()?;
                out = Some(value.value());
            } else if meta.input.peek(Token![=]) {
                let _: syn::Expr = meta.value()?.parse()?;
            }
            Ok(())
        })?;
    }
    Ok(out)
}

struct EnumValue {
    wire: String,
    ident: String,
    docs: Vec<String>,
}

/// `Some(values)` if `e` is a unit-only enum with derived serde (typify's string enums).
fn string_enum_values(e: &ItemEnum) -> Result<Option<Vec<EnumValue>>> {
    if !has_derive(&e.attrs, "Deserialize")? {
        return Ok(None);
    }
    let flags = serde_flags(&e.attrs)?;
    if flags.contains("untagged") || flags.contains("tag") || flags.contains("content") {
        return Ok(None);
    }
    let mut values = Vec::new();
    for v in &e.variants {
        if !matches!(v.fields, Fields::Unit) || v.discriminant.is_some() {
            return Ok(None);
        }
        let wire = serde_rename(&v.attrs)?.unwrap_or_else(|| v.ident.to_string());
        let other_attrs = v
            .attrs
            .iter()
            .filter(|a| !a.path().is_ident("doc") && !a.path().is_ident("serde"))
            .count();
        if other_attrs > 0 {
            return Ok(None);
        }
        values.push(EnumValue {
            wire,
            ident: v.ident.to_string(),
            docs: doc_lines(&v.attrs),
        });
    }
    Ok(Some(values))
}

/// Names inside every `#[serde(...)]`, e.g. `{"rename", "untagged", "default"}`.
fn serde_flags(attrs: &[Attribute]) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for attr in attrs.iter().filter(|a| a.path().is_ident("serde")) {
        attr.parse_nested_meta(|meta| {
            if let Some(ident) = meta.path.get_ident() {
                out.insert(ident.to_string());
            }
            if meta.input.peek(Token![=]) {
                let _: syn::Expr = meta.value()?.parse()?;
            }
            Ok(())
        })?;
    }
    Ok(out)
}

fn serde_rename(attrs: &[Attribute]) -> Result<Option<String>> {
    let mut out = None;
    for attr in attrs.iter().filter(|a| a.path().is_ident("serde")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename") {
                let value: LitStr = meta.value()?.parse()?;
                out = Some(value.value());
            } else if meta.input.peek(Token![=]) {
                let _: syn::Expr = meta.value()?.parse()?;
            }
            Ok(())
        })?;
    }
    Ok(out)
}

fn derive_paths(attr: &Attribute) -> Result<Vec<Path>> {
    Ok(attr
        .parse_args_with(Punctuated::<Path, Token![,]>::parse_terminated)?
        .into_iter()
        .collect())
}

fn path_is(path: &Path, name: &str) -> bool {
    path.segments.last().is_some_and(|s| s.ident == name)
}

fn has_derive(attrs: &[Attribute], name: &str) -> Result<bool> {
    for attr in attrs.iter().filter(|a| a.path().is_ident("derive")) {
        if derive_paths(attr)?.iter().any(|p| path_is(p, name)) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Removes `name` from `#[derive(...)]`; true if it was there.
fn remove_derive(attrs: &mut [Attribute], name: &str) -> Result<bool> {
    let mut removed = false;
    for attr in attrs.iter_mut().filter(|a| a.path().is_ident("derive")) {
        let paths = derive_paths(attr)?;
        if paths.iter().any(|p| path_is(p, name)) {
            let kept = paths.into_iter().filter(|p| !path_is(p, name));
            *attr = parse_quote!(#[derive(#(#kept),*)]);
            removed = true;
        }
    }
    Ok(removed)
}

/// Last path segment of a plain path type, e.g. `PullRequestAction` for
/// `types::PullRequestAction`.
fn type_ident(ty: &Type) -> Option<String> {
    match ty {
        Type::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
        _ => None,
    }
}

/// `Some(T)` if `ty` is `<..>::name<T>`.
fn generic_arg<'a>(ty: &'a Type, name: &str) -> Option<&'a Type> {
    let Type::Path(p) = ty else { return None };
    let last = p.path.segments.last()?;
    if last.ident != name {
        return None;
    }
    let syn::PathArguments::AngleBracketed(args) = &last.arguments else {
        return None;
    };
    match args.args.first()? {
        syn::GenericArgument::Type(t) if args.args.len() == 1 => Some(t),
        _ => None,
    }
}

fn decoder_for(ty: &Type, kind: &str) -> Option<&'static str> {
    // Elements that are tagged unions also drop their `Unrecognized` values: T3's
    // `ForwardCompatibleUnionArray` (and its open union) drops members with unknown tags.
    let tagged = |element: &Type| {
        element
            .to_token_stream()
            .to_string()
            .starts_with("crate :: tagged ::")
    };
    match kind {
        "array" => {
            if let Some(element) = generic_arg(ty, "Vec") {
                return Some(if tagged(element) { "known_vec" } else { "vec" });
            }
            let element = generic_arg(generic_arg(ty, "Option")?, "Vec")?;
            Some(if tagged(element) {
                "option_known_vec"
            } else {
                "option_vec"
            })
        }
        "option" => generic_arg(ty, "Option").map(|_| "option"),
        _ => None,
    }
}

/// Every identifier mentioned in `tokens`.
fn idents_in(tokens: &impl ToTokens) -> BTreeSet<String> {
    fn walk(stream: TokenStream, out: &mut BTreeSet<String>) {
        for tree in stream {
            match tree {
                TokenTree::Ident(i) => {
                    out.insert(i.to_string());
                }
                TokenTree::Group(g) => walk(g.stream(), out),
                _ => {}
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(tokens.to_token_stream(), &mut out);
    out
}

/// `match *self { .. }` -> `match self { .. }` (the type is no longer `Copy`).
struct MatchSelfByRef;

impl VisitMut for MatchSelfByRef {
    fn visit_expr_match_mut(&mut self, m: &mut syn::ExprMatch) {
        if let syn::Expr::Unary(u) = &*m.expr
            && matches!(u.op, syn::UnOp::Deref(_))
            && quote!(#u).to_string() == "* self"
        {
            *m.expr = parse_quote!(self);
        }
        syn::visit_mut::visit_expr_match_mut(self, m);
    }
}

/// Renders doc attributes (`#[doc = "x"]`) as `///x` lines.
pub fn doc_lines(attrs: &[Attribute]) -> Vec<String> {
    let mut out = Vec::new();
    for attr in attrs.iter().filter(|a| a.path().is_ident("doc")) {
        if let syn::Meta::NameValue(nv) = &attr.meta
            && let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) = &nv.value
        {
            for line in s.value().split('\n') {
                out.push(format!("///{line}"));
            }
        }
    }
    out
}
