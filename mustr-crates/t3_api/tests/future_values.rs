//! Synthetic values from a T3 that is newer than any real one: every sample in
//! `fixtures/samples/` (this build's own schemas), with additions a newer server could make
//! anywhere, guided by the schema:
//!
//! - an unknown field on every object, which must change nothing;
//! - an extra element in every list of structs, tagged unions or string enums: one with an
//!   unknown tag or value, or a struct this build can't read at all, which must not fail the
//!   list;
//! - an unknown tag or enum value in place of each known one, one at a time, including optional
//!   constants a newer T3 could widen, which must not fail the value around it.
//!
//! Every mutated value must decode. `tests/newer_server.rs` does the same with a real newer T3's
//! schemas.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::Path,
};

use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};

mod dispatch {
    use super::rt;
    include!("../src/generated/dispatch.rs");
}

fn rt<T: DeserializeOwned + Serialize>(v: Value) -> Result<Value, String> {
    let t: T = serde_json::from_value(v).map_err(|e| e.to_string())?;
    serde_json::to_value(&t).map_err(|e| e.to_string())
}

const FUTURE_FIELD: &str = "zzFutureField";
const FUTURE_VALUE: &str = "zz-future-value";

/// A place in a sample where a newer server could put something this build doesn't know.
#[derive(Clone, Debug)]
enum Site {
    /// A struct (an object with declared properties, not a record).
    Struct { path: Vec<Step> },
    /// A tagged union (`taggedUnions` in the manifest), with its tag field.
    Tagged { path: Vec<Step>, tag: String },
    /// A string enum with two or more values.
    Enum { path: Vec<Step> },
    /// A list of structs, tagged unions or string enums: `element` is an element a newer T3
    /// could add.
    List { path: Vec<Step>, element: Value },
}

#[derive(Clone, Debug)]
enum Step {
    Key(String),
    Index(usize),
}

struct Schema {
    definitions: Map<String, Value>,
    /// Tagged union definition -> its tag field.
    tagged: HashMap<String, String>,
}

impl Schema {
    fn load() -> Self {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("schema");
        let schema: Value =
            serde_json::from_str(&fs::read_to_string(dir.join("t3-api.schema.json")).unwrap())
                .unwrap();
        let manifest: Value =
            serde_json::from_str(&fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
        let tagged = manifest["taggedUnions"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(def, union)| (def.clone(), union["tag"].as_str().unwrap().to_string()))
            .collect();
        Self {
            definitions: schema["definitions"].as_object().unwrap().clone(),
            tagged,
        }
    }

    /// Follows `$ref`s; returns the schema and the last definition's name.
    fn resolve<'a>(&'a self, mut schema: &'a Value) -> (&'a Value, Option<&'a str>) {
        let mut name = None;
        while let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
            let def = reference.trim_start_matches("#/definitions/");
            let (key, value) = self
                .definitions
                .get_key_value(def)
                .unwrap_or_else(|| panic!("no definition {def}"));
            name = Some(key.as_str());
            schema = value;
        }
        (schema, name)
    }

    /// Whether `value` plausibly is an instance of `schema` (to pick an untagged union's
    /// member): the right JSON type, its required keys, and its constant fields.
    fn fits(&self, value: &Value, schema: &Value) -> bool {
        let (schema, name) = self.resolve(schema);
        if let Some(tag) = name.and_then(|name| self.tagged.get(name)) {
            return value.get(tag).is_some();
        }
        if let Some(members) = schema.get("oneOf").and_then(Value::as_array) {
            return members.iter().any(|member| self.fits(value, member));
        }
        if let Some(values) = schema.get("enum").and_then(Value::as_array) {
            return values.contains(value) || (values.len() >= 2 && value.is_string());
        }
        match schema.get("type").and_then(Value::as_str) {
            Some("object") => {
                let Some(object) = value.as_object() else {
                    return false;
                };
                let required = schema
                    .get("required")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str);
                let constants_match = schema
                    .get("properties")
                    .and_then(Value::as_object)
                    .into_iter()
                    .flatten()
                    .all(|(key, property)| {
                        let (property, _) = self.resolve(property);
                        match (
                            property.get("enum").and_then(Value::as_array),
                            object.get(key),
                        ) {
                            (Some(values), Some(actual)) if values.len() == 1 => {
                                values[0] == *actual
                            }
                            _ => true,
                        }
                    });
                required.into_iter().all(|key| object.contains_key(key)) && constants_match
            }
            Some("array") => value.is_array(),
            Some("string") => value.is_string(),
            Some("boolean") => value.is_boolean(),
            Some("integer") | Some("number") => value.is_number() || value.is_string(),
            Some("null") => value.is_null(),
            _ => true,
        }
    }

    /// Every [`Site`] in `value`, an instance of `schema`.
    fn sites(&self, value: &Value, schema: &Value, path: &mut Vec<Step>, out: &mut Vec<Site>) {
        let (schema, name) = self.resolve(schema);
        if let Some(tag) = name.and_then(|name| self.tagged.get(name)) {
            if !path.is_empty() {
                out.push(Site::Tagged {
                    path: path.clone(),
                    tag: tag.clone(),
                });
            }
            // Into the member the tag names.
            let members = schema.get("oneOf").and_then(Value::as_array);
            if let Some(member) = members
                .into_iter()
                .flatten()
                .find(|member| self.fits(value, member))
            {
                self.sites_within(value, self.resolve(member).0, path, out);
            }
            return;
        }
        if let Some(members) = schema.get("oneOf").and_then(Value::as_array) {
            if let Some(member) = members.iter().find(|member| self.fits(value, member)) {
                self.sites(value, member, path, out);
            }
            return;
        }
        if let Some(values) = schema.get("enum").and_then(Value::as_array) {
            if values.len() >= 2 && value.is_string() {
                out.push(Site::Enum { path: path.clone() });
            }
            return;
        }
        self.sites_within(value, schema, path, out);
    }

    /// Whether `schema` is a single-value constant, such as `Literal(2)`.
    fn is_constant(&self, schema: &Value) -> bool {
        let (schema, _) = self.resolve(schema);
        schema
            .get("enum")
            .and_then(Value::as_array)
            .is_some_and(|values| values.len() == 1)
    }

    fn sites_within(
        &self,
        value: &Value,
        schema: &Value,
        path: &mut Vec<Step>,
        out: &mut Vec<Site>,
    ) {
        match value {
            Value::Object(object) => {
                let properties = schema.get("properties").and_then(Value::as_object);
                if properties.is_some() {
                    out.push(Site::Struct { path: path.clone() });
                }
                // An optional constant a newer T3 could widen (`Literal(2)` to
                // `Literals([2, 3])`).
                let required: Vec<&str> = schema
                    .get("required")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                for (key, property) in properties.into_iter().flatten() {
                    if object.contains_key(key)
                        && !required.contains(&key.as_str())
                        && self.is_constant(property)
                    {
                        let mut path = path.clone();
                        path.push(Step::Key(key.clone()));
                        out.push(Site::Enum { path });
                    }
                }
                let records = schema.get("additionalProperties").filter(|a| a.is_object());
                for (key, field) in object {
                    let Some(field_schema) = properties.and_then(|p| p.get(key)).or(records) else {
                        continue;
                    };
                    path.push(Step::Key(key.clone()));
                    self.sites(field, field_schema, path, out);
                    path.pop();
                }
            }
            Value::Array(elements) => {
                let Some(items) = schema.get("items") else {
                    return;
                };
                if let Some(element) = self.future_element(elements, items) {
                    out.push(Site::List {
                        path: path.clone(),
                        element,
                    });
                }
                for (index, element) in elements.iter().enumerate() {
                    path.push(Step::Index(index));
                    self.sites(element, items, path, out);
                    path.pop();
                }
            }
            _ => {}
        }
    }

    /// An element a newer server could add to a list of `items`: an unknown member of a tagged
    /// union (a known one with its tag changed), an unknown enum value, or a struct whose shape
    /// changed so this build can't read it.
    fn future_element(&self, elements: &[Value], items: &Value) -> Option<Value> {
        let (items, name) = self.resolve(items);
        if let Some(tag) = name.and_then(|name| self.tagged.get(name)) {
            let mut element = elements
                .first()
                .cloned()
                .filter(Value::is_object)
                .unwrap_or_else(|| json!({}));
            element[tag.as_str()] = json!(FUTURE_VALUE);
            return Some(element);
        }
        if let Some(values) = items.get("enum").and_then(Value::as_array) {
            return (values.len() >= 2).then(|| json!(FUTURE_VALUE));
        }
        let required = items.get("required").and_then(Value::as_array)?;
        (!required.is_empty()).then(|| json!({ FUTURE_FIELD: "a struct missing its fields" }))
    }
}

fn at<'a>(value: &'a mut Value, path: &[Step]) -> &'a mut Value {
    path.iter().fold(value, |value, step| match step {
        Step::Key(key) => &mut value[key.as_str()],
        Step::Index(index) => &mut value[*index],
    })
}

/// Adds [`FUTURE_FIELD`] to every struct.
fn with_future_fields(value: &Value, sites: &[Site]) -> Value {
    let mut value = value.clone();
    for site in sites {
        if let Site::Struct { path } = site
            && let Value::Object(object) = at(&mut value, path)
        {
            object.insert(FUTURE_FIELD.into(), json!({ "nested": [1, "two", null] }));
        }
    }
    value
}

/// Every value to mutate, as `(definition, label, value)`: the Arbitrary samples, T3's test
/// fixtures, and what a newer server sent (`newer-server/live/`).
fn corpus() -> Vec<(String, String, Value)> {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut corpus = Vec::new();
    for (dir, many) in [
        ("samples", true),
        ("t3-tests", false),
        ("newer-server/live", true),
    ] {
        let Ok(entries) = fs::read_dir(fixtures.join(dir)) else {
            continue;
        };
        let mut files: Vec<_> = entries
            .map(|e| e.unwrap().path())
            .filter(|p| !p.to_string_lossy().ends_with(".expected.json"))
            .collect();
        files.sort();
        for path in files {
            let name = path.file_name().unwrap().to_str().unwrap().to_string();
            let def = name.split('.').next().unwrap().to_string();
            let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            match value {
                Value::Array(values) if many => {
                    for (i, value) in values.into_iter().enumerate() {
                        corpus.push((def.clone(), format!("{dir}/{name}[{i}]"), value));
                    }
                }
                value => corpus.push((def, format!("{dir}/{name}"), value)),
            }
        }
    }
    corpus
}

/// Drops [`FUTURE_FIELD`] wherever an untyped (`serde_json::Value`) part kept it, and the
/// `null`s and number spellings serde and Effect write differently.
fn normalized(value: &Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .filter(|(key, value)| *key != FUTURE_FIELD && !value.is_null())
                .map(|(key, value)| (key.clone(), normalized(value)))
                .collect(),
        ),
        Value::Array(elements) => Value::Array(elements.iter().map(normalized).collect()),
        Value::Number(number) => serde_json::Number::from_f64(number.as_f64().unwrap())
            .map(Value::Number)
            .unwrap_or(Value::Null),
        other => other.clone(),
    }
}

#[test]
fn future_additions_never_fail_a_decode() {
    let schema = Schema::load();
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut failures: BTreeMap<String, String> = BTreeMap::new();
    for (def, label, sample) in corpus() {
        let Some(def_schema) = schema.definitions.get(&def).cloned() else {
            continue;
        };
        let Some(Ok(baseline)) = dispatch::roundtrip(&def, sample.clone()) else {
            continue;
        };
        let mut fail = |what: &str, error: String| {
            failures
                .entry(format!("{def} ({what})"))
                .or_insert(format!("{label}: {error}"));
        };
        let mut sites = Vec::new();
        schema.sites(&sample, &def_schema, &mut Vec::new(), &mut sites);

        // An unknown field on every struct changes nothing.
        *counts.entry("unknown fields").or_default() += 1;
        match dispatch::roundtrip(&def, with_future_fields(&sample, &sites)).unwrap() {
            Ok(out) if normalized(&out) == normalized(&baseline) => {}
            Ok(out) => fail("unknown fields", format!("decoded differently: {out}")),
            Err(error) => fail("unknown fields", error),
        }

        // An unknown element in every list at once, deepest first so the paths stay valid.
        let mut lists = sample.clone();
        let mut any_list = false;
        for site in sites.iter().rev() {
            if let Site::List { path, element } = site
                && let Value::Array(elements) = at(&mut lists, path)
            {
                elements.push(element.clone());
                any_list = true;
            }
        }
        if any_list {
            *counts.entry("unknown list elements").or_default() += 1;
            if let Err(error) = dispatch::roundtrip(&def, lists).unwrap() {
                fail("unknown list elements", error);
            }
        }

        // Each known tag or enum value replaced by an unknown one, one at a time.
        for site in &sites {
            let mut mutated = sample.clone();
            let what = match site {
                Site::Tagged { path, tag } => {
                    at(&mut mutated, path)[tag.as_str()] = json!(FUTURE_VALUE);
                    "unknown tag"
                }
                Site::Enum { path } => {
                    let value = at(&mut mutated, path);
                    // A widened constant keeps its JSON type: `true` may become `false`, `2` may
                    // become `3`.
                    *value = match value {
                        Value::Bool(constant) => json!(!*constant),
                        Value::Number(constant) => json!(constant.as_f64().unwrap_or(0.) + 1.),
                        _ => json!(FUTURE_VALUE),
                    };
                    "unknown enum value"
                }
                Site::List { .. } | Site::Struct { .. } => continue,
            };
            *counts.entry(what).or_default() += 1;
            if let Err(error) = dispatch::roundtrip(&def, mutated).unwrap() {
                fail(what, error);
            }
        }
    }

    eprintln!("future values decoded: {counts:?}");
    for (key, error) in &failures {
        eprintln!("  FAIL {key}: {error}");
    }
    assert!(counts.values().sum::<usize>() > 3000, "{counts:?}");
    assert!(
        failures.is_empty(),
        "{} types fail on values a newer T3 could send",
        failures.len()
    );
}
