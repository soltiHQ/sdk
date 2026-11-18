//! # Podium's draft 2020-12 subset
//!
//! The walk mirrors Podium's compiler (`internal/workloadschema/compile.go` at
//! commit 222f2b0), position for position and keyword for keyword:
//!
//! - It compiles the tree of subschemas first, from the root through `$defs`,
//!   `properties`, `patternProperties` and `dependentSchemas` values, the items
//!   of `allOf`, `anyOf`, `oneOf` and `prefixItems`, and `not`, `if`, `then`,
//!   `else`, `additionalProperties`, `propertyNames`, `items` and `contains`.
//!   Keys are visited in byte order. The tree registers each `$anchor`.
//! - Then it resolves each `$ref`, compiling a target that is not in the tree,
//!   such as one under a keyword the subset does not know.
//! - Last, it refuses a cycle of subschemas at one location of a body.
//!
//! A subschema is compiled once for each position, the object or array that
//! holds it and its key there, as Podium keys its compiled subschemas.
//! Unknown keywords and `x-*` keywords are ignored, and so are the values of
//! annotations; none of them is compiled unless a `$ref` points into it.
//!
//! The walk collects the regexes it meets, in order, for the regex checks.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    rc::Rc,
};

use serde_json::{Map, Value};

use super::{WORKLOAD_SCHEMA_DIALECT, WorkloadSchemaError, WorkloadSchemaErrorReason as Reason};

/// The draft 2020-12 and 2019-09 keywords the subset leaves out, and why.
const REFUSED: [(&str, &str); 7] = [
    ("$dynamicRef", "it resolves at run time"),
    ("$dynamicAnchor", "it resolves at run time"),
    ("$recursiveRef", "it resolves at run time"),
    ("$recursiveAnchor", "it resolves at run time"),
    ("$vocabulary", "it belongs to metaschemas"),
    (
        "unevaluatedItems",
        "it depends on what the keywords beside it evaluated",
    ),
    (
        "unevaluatedProperties",
        "it depends on what the keywords beside it evaluated",
    ),
];

/// The keywords of the subset.
const KEYWORDS: [&str; 52] = [
    "$schema",
    "$id",
    "$anchor",
    "$ref",
    "$defs",
    "$comment",
    "title",
    "description",
    "format",
    "contentEncoding",
    "contentMediaType",
    "deprecated",
    "readOnly",
    "writeOnly",
    "examples",
    "default",
    "contentSchema",
    "type",
    "enum",
    "const",
    "multipleOf",
    "maximum",
    "exclusiveMaximum",
    "minimum",
    "exclusiveMinimum",
    "maxLength",
    "minLength",
    "maxItems",
    "minItems",
    "maxContains",
    "minContains",
    "maxProperties",
    "minProperties",
    "pattern",
    "uniqueItems",
    "required",
    "dependentRequired",
    "allOf",
    "anyOf",
    "oneOf",
    "prefixItems",
    "not",
    "if",
    "then",
    "else",
    "additionalProperties",
    "propertyNames",
    "items",
    "contains",
    "properties",
    "dependentSchemas",
    "patternProperties",
];

const A_STRING: &str = "a string";
const A_BOOLEAN: &str = "a boolean";
const AN_ARRAY: &str = "an array";
const A_NUMBER: &str = "a number";
const A_POSITIVE_NUMBER: &str = "a number greater than zero";
const A_COUNT: &str = "a non-negative integer";
const A_SCHEMA: &str = "a schema: an object or a boolean";
const AN_ITEMS_SCHEMA: &str = "a schema: an object or a boolean (in draft 2020-12, \
     prefixItems holds the schemas of the first items)";
const AN_OBJECT_OF_SCHEMAS: &str = "an object whose values are schemas: objects or booleans";
const AN_ARRAY_OF_SCHEMAS: &str = "a non-empty array of schemas: objects or booleans";
const A_TYPE: &str = "a type name (null, boolean, object, array, number, string or integer), \
     or a non-empty array of distinct type names";
const NAMES: &str = "an array of distinct strings";
const DEPENDENT_NAMES: &str = "an object whose values are arrays of distinct strings";
const AN_ANCHOR: &str = "a plain name: a letter or `_`, then letters, digits, `-`, `_` and `.`";

/// A regex the walk met, and the subschema whose keyword holds it.
pub(super) struct Pattern<'a> {
    pub(super) at: Location,
    pub(super) text: &'a str,
}

/// Where a value is in the schema: the location that holds it, and its RFC
/// 6901 token there. The root has no token. It is written out only for an
/// error, so that no subschema keeps the names of its ancestors.
#[derive(Clone, Default)]
pub(super) struct Location(Option<Rc<Step>>);

struct Step {
    parent: Location,
    token: String,
}

impl Location {
    fn child(&self, token: &str) -> Self {
        Self(Some(Rc::new(Step {
            parent: self.clone(),
            token: token.to_owned(),
        })))
    }

    fn is_root(&self) -> bool {
        self.0.is_none()
    }
}

impl fmt::Display for Location {
    /// Writes the location as a URI fragment, `#/properties/name`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut tokens = Vec::new();
        let mut at = &self.0;
        while let Some(step) = at {
            tokens.push(step.token.as_str());
            at = &step.parent.0;
        }
        f.write_str("#")?;
        for token in tokens.iter().rev() {
            write!(f, "/{}", token.replace('~', "~0").replace('/', "~1"))?;
        }
        Ok(())
    }
}

/// The position of a value in the document, by which a subschema compiled
/// there is found again: the object or array that holds it, by address, and
/// its member name or index there. The root's is `(0, "")`.
type Position = (usize, String);

const ROOT: usize = 0;

fn position_in<T>(container: &T, key: &str) -> Position {
    (std::ptr::from_ref(container).addr(), key.to_owned())
}

/// A compiled subschema: where it is, and the subschemas it evaluates at the
/// value it validates, for the cycle check.
struct Node {
    at: Location,
    reference: Option<usize>,
    not: Option<usize>,
    if_: Option<usize>,
    then: Option<usize>,
    else_: Option<usize>,
    all_of: Vec<usize>,
    any_of: Vec<usize>,
    one_of: Vec<usize>,
    dependent: Vec<usize>,
}

impl Node {
    fn new(at: Location) -> Self {
        Self {
            at,
            reference: None,
            not: None,
            if_: None,
            then: None,
            else_: None,
            all_of: Vec::new(),
            any_of: Vec::new(),
            one_of: Vec::new(),
            dependent: Vec::new(),
        }
    }

    /// The subschemas it evaluates at the value it validates, in Podium's
    /// order. `then` and `else` apply only beside an `if`.
    fn same_location(&self) -> Vec<usize> {
        let mut out: Vec<usize> = [self.reference, self.not, self.if_]
            .into_iter()
            .flatten()
            .collect();
        if self.if_.is_some() {
            out.extend([self.then, self.else_].into_iter().flatten());
        }
        out.extend(&self.all_of);
        out.extend(&self.any_of);
        out.extend(&self.one_of);
        out.extend(&self.dependent);
        out
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Visit {
    New,
    Visiting,
    Done,
}

/// Walks a schema object as Podium's compiler does, and returns its regexes in
/// the order the walk met them.
pub(super) fn check(doc: &Value) -> Result<Vec<Pattern<'_>>, WorkloadSchemaError> {
    let mut compiler = Compiler {
        doc,
        nodes: Vec::new(),
        by_position: HashMap::new(),
        anchors: HashMap::new(),
        refs: Vec::new(),
        patterns: Vec::new(),
    };
    compiler.compile(doc, Location::default(), (0, String::new()), true)?;
    // A target compiled here may add more references.
    let mut i = 0;
    while i < compiler.refs.len() {
        let (from, reference) = compiler.refs[i];
        let target = compiler.resolve(from, reference)?;
        compiler.nodes[from].reference = Some(target);
        i += 1;
    }
    compiler.check_cycles()?;
    Ok(compiler.patterns)
}

struct Compiler<'a> {
    doc: &'a Value,
    nodes: Vec<Node>,
    by_position: HashMap<Position, usize>,
    anchors: HashMap<&'a str, usize>,
    refs: Vec<(usize, &'a str)>,
    patterns: Vec<Pattern<'a>>,
}

impl<'a> Compiler<'a> {
    /// Compiles the subschema `value`, an object or a boolean, unless the
    /// position already has one. A subschema of the tree registers its
    /// `$anchor`.
    fn compile(
        &mut self,
        value: &'a Value,
        at: Location,
        position: Position,
        tree: bool,
    ) -> Result<usize, WorkloadSchemaError> {
        if let Some(&id) = self.by_position.get(&position) {
            return Ok(id);
        }
        let id = self.nodes.len();
        self.nodes.push(Node::new(at));
        self.by_position.insert(position, id);
        if let Value::Object(object) = value {
            let mut keys: Vec<&'a String> = object.keys().collect();
            keys.sort_unstable();
            for key in keys {
                self.keyword(id, object, key, tree)?;
            }
        }
        Ok(id)
    }

    /// Checks the keyword `key` of the subschema `id`, whose object is
    /// `object`, and compiles the subschemas it holds.
    fn keyword(
        &mut self,
        id: usize,
        object: &'a Map<String, Value>,
        key: &'a str,
        tree: bool,
    ) -> Result<(), WorkloadSchemaError> {
        let value = &object[key];
        let at = self.nodes[id].at.clone();
        let fail = |reason: Reason| Err(WorkloadSchemaError::new(at.to_string(), reason));

        let Some(keyword) = KEYWORDS.into_iter().find(|name| *name == key) else {
            return match REFUSED.iter().find(|(name, _)| *name == key) {
                Some(&(keyword, why)) => fail(Reason::UnsupportedKeyword { keyword, why }),
                // A keyword that begins with "x-", or that the subset does not know.
                None => Ok(()),
            };
        };
        let invalid =
            |expected: &'static str| fail(Reason::InvalidKeywordValue { keyword, expected });

        match keyword {
            "$schema" => match value {
                Value::String(text) if text == WORKLOAD_SCHEMA_DIALECT => Ok(()),
                Value::String(text) => fail(Reason::UnsupportedDialect {
                    value: text.clone(),
                }),
                other => fail(Reason::UnsupportedDialect {
                    value: other.to_string(),
                }),
            },
            "$id" => {
                if !at.is_root() {
                    return fail(Reason::NestedId);
                }
                if value.is_string() {
                    Ok(())
                } else {
                    invalid(A_STRING)
                }
            }
            "$anchor" => {
                let Some(name) = value.as_str().filter(|name| is_anchor_name(name)) else {
                    return invalid(AN_ANCHOR);
                };
                if tree {
                    if self.anchors.contains_key(name) {
                        return fail(Reason::DuplicateAnchor {
                            anchor: name.to_owned(),
                        });
                    }
                    self.anchors.insert(name, id);
                }
                Ok(())
            }
            "$ref" => {
                let Some(reference) = value.as_str() else {
                    return invalid(A_STRING);
                };
                if !reference.starts_with('#') {
                    return fail(Reason::RemoteReference {
                        reference: reference.to_owned(),
                    });
                }
                self.refs.push((id, reference));
                Ok(())
            }
            "$comment" | "title" | "description" | "format" | "contentEncoding"
            | "contentMediaType" => {
                if value.is_string() {
                    Ok(())
                } else {
                    invalid(A_STRING)
                }
            }
            "deprecated" | "readOnly" | "writeOnly" | "uniqueItems" => {
                if value.is_boolean() {
                    Ok(())
                } else {
                    invalid(A_BOOLEAN)
                }
            }
            "examples" | "enum" => {
                if value.is_array() {
                    Ok(())
                } else {
                    invalid(AN_ARRAY)
                }
            }
            "default" | "const" => Ok(()),
            "contentSchema" => {
                if is_schema(value) {
                    Ok(())
                } else {
                    invalid(A_SCHEMA)
                }
            }
            "type" => {
                if is_types(value) {
                    Ok(())
                } else {
                    invalid(A_TYPE)
                }
            }
            "multipleOf" => {
                // Canonical JSON writes each number as the double it holds.
                if value.as_f64().is_some_and(|number| number > 0.0) {
                    Ok(())
                } else {
                    invalid(A_POSITIVE_NUMBER)
                }
            }
            "maximum" | "exclusiveMaximum" | "minimum" | "exclusiveMinimum" => {
                if value.is_number() {
                    Ok(())
                } else {
                    invalid(A_NUMBER)
                }
            }
            "maxLength" | "minLength" | "maxItems" | "minItems" | "maxContains" | "minContains"
            | "maxProperties" | "minProperties" => {
                let count = value
                    .as_f64()
                    .is_some_and(|number| number >= 0.0 && number.fract() == 0.0);
                if count { Ok(()) } else { invalid(A_COUNT) }
            }
            "pattern" => {
                let Some(text) = value.as_str() else {
                    return invalid(A_STRING);
                };
                self.patterns.push(Pattern { at, text });
                Ok(())
            }
            "required" => {
                if is_names(value) {
                    Ok(())
                } else {
                    invalid(NAMES)
                }
            }
            "dependentRequired" => {
                let names = value
                    .as_object()
                    .is_some_and(|deps| deps.values().all(is_names));
                if names {
                    Ok(())
                } else {
                    invalid(DEPENDENT_NAMES)
                }
            }
            "allOf" | "anyOf" | "oneOf" | "prefixItems" => {
                let Some(list) = value.as_array().filter(|list| !list.is_empty()) else {
                    return invalid(AN_ARRAY_OF_SCHEMAS);
                };
                let mut ids = Vec::with_capacity(list.len());
                for (index, item) in list.iter().enumerate() {
                    let index = index.to_string();
                    let item_at = at.child(keyword).child(&index);
                    if !is_schema(item) {
                        return Err(WorkloadSchemaError::new(
                            item_at.to_string(),
                            Reason::InvalidKeywordValue {
                                keyword,
                                expected: AN_ARRAY_OF_SCHEMAS,
                            },
                        ));
                    }
                    ids.push(self.compile(item, item_at, position_in(list, &index), tree)?);
                }
                let node = &mut self.nodes[id];
                match keyword {
                    "allOf" => node.all_of = ids,
                    "anyOf" => node.any_of = ids,
                    "oneOf" => node.one_of = ids,
                    _ => {}
                }
                Ok(())
            }
            "not"
            | "if"
            | "then"
            | "else"
            | "additionalProperties"
            | "propertyNames"
            | "items"
            | "contains" => {
                if !is_schema(value) {
                    return invalid(if keyword == "items" {
                        AN_ITEMS_SCHEMA
                    } else {
                        A_SCHEMA
                    });
                }
                let one =
                    self.compile(value, at.child(keyword), position_in(object, keyword), tree)?;
                let node = &mut self.nodes[id];
                match keyword {
                    "not" => node.not = Some(one),
                    "if" => node.if_ = Some(one),
                    "then" => node.then = Some(one),
                    "else" => node.else_ = Some(one),
                    _ => {}
                }
                Ok(())
            }
            "$defs" | "properties" | "dependentSchemas" | "patternProperties" => {
                let Some(members) = value.as_object() else {
                    return invalid(AN_OBJECT_OF_SCHEMAS);
                };
                let mut names: Vec<&'a String> = members.keys().collect();
                names.sort_unstable();
                let mut ids = Vec::with_capacity(names.len());
                for name in names {
                    let member = &members[name.as_str()];
                    if keyword == "patternProperties" {
                        self.patterns.push(Pattern {
                            at: at.clone(),
                            text: name.as_str(),
                        });
                    }
                    let member_at = at.child(keyword).child(name);
                    if !is_schema(member) {
                        return Err(WorkloadSchemaError::new(
                            member_at.to_string(),
                            Reason::InvalidKeywordValue {
                                keyword,
                                expected: AN_OBJECT_OF_SCHEMAS,
                            },
                        ));
                    }
                    ids.push(self.compile(member, member_at, position_in(members, name), tree)?);
                }
                if keyword == "dependentSchemas" {
                    self.nodes[id].dependent = ids;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Finds the subschema that a `$ref` inside the document points at, and
    /// compiles it if the tree did not.
    fn resolve(&mut self, from: usize, reference: &'a str) -> Result<usize, WorkloadSchemaError> {
        let unresolved = || {
            WorkloadSchemaError::new(
                self.nodes[from].at.to_string(),
                Reason::UnresolvedReference {
                    reference: reference.to_owned(),
                },
            )
        };
        let fragment = &reference[1..];
        if fragment.is_empty() {
            return Ok(ROOT);
        }
        if !fragment.starts_with('/') {
            if !is_anchor_name(fragment) {
                return Err(unresolved());
            }
            return self.anchors.get(fragment).copied().ok_or_else(unresolved);
        }

        // A URI fragment is percent-decoded first, then read as an RFC 6901
        // pointer. Text that is not UTF-8 names no member and no index.
        let decoded = percent_decode(fragment)
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .ok_or_else(unresolved)?;
        let mut tokens = Vec::new();
        for token in decoded[1..].split('/') {
            if token.replace("~0", "").replace("~1", "").contains('~') {
                return Err(unresolved());
            }
            tokens.push(token.replace("~1", "/").replace("~0", "~"));
        }

        let mut value = self.doc;
        let mut position = (0, String::new());
        for token in &tokens {
            match value {
                Value::Object(members) => {
                    let Some(child) = members.get(token) else {
                        return Err(unresolved());
                    };
                    position = position_in(members, token);
                    value = child;
                }
                Value::Array(items) => {
                    let Some(index) = array_index(token, items.len()) else {
                        return Err(unresolved());
                    };
                    position = position_in(items, token);
                    value = &items[index];
                }
                _ => return Err(unresolved()),
            }
        }
        if !is_schema(value) {
            return Err(unresolved());
        }
        let at = tokens
            .iter()
            .fold(Location::default(), |at, token| at.child(token));
        self.compile(value, at, position, false)
    }

    /// Refuses a cycle of subschemas through the keywords that evaluate a
    /// subschema at the value they validate.
    fn check_cycles(&self) -> Result<(), WorkloadSchemaError> {
        let mut state = vec![Visit::New; self.nodes.len()];
        for id in 0..self.nodes.len() {
            self.visit(id, &mut state)?;
        }
        Ok(())
    }

    fn visit(&self, id: usize, state: &mut [Visit]) -> Result<(), WorkloadSchemaError> {
        match state[id] {
            Visit::Visiting => {
                return Err(WorkloadSchemaError::new(
                    self.nodes[id].at.to_string(),
                    Reason::ReferenceCycle,
                ));
            }
            Visit::Done => return Ok(()),
            Visit::New => {}
        }
        state[id] = Visit::Visiting;
        for next in self.nodes[id].same_location() {
            self.visit(next, state)?;
        }
        state[id] = Visit::Done;
        Ok(())
    }
}

/// Reports a value that may be a schema: an object or a boolean.
fn is_schema(value: &Value) -> bool {
    matches!(value, Value::Object(_) | Value::Bool(_))
}

/// Reports a `type` value: a type name, or a non-empty array of distinct ones.
fn is_types(value: &Value) -> bool {
    let names: Vec<&Value> = match value {
        Value::String(_) => vec![value],
        Value::Array(items) if !items.is_empty() => items.iter().collect(),
        _ => return false,
    };
    let mut seen = 0_u8;
    for name in names {
        let bit = match name.as_str() {
            Some("null") => 1,
            Some("boolean") => 2,
            Some("number") => 4,
            Some("string") => 8,
            Some("array") => 16,
            Some("object") => 32,
            Some("integer") => 64,
            _ => return false,
        };
        if seen & bit != 0 {
            return false;
        }
        seen |= bit;
    }
    true
}

/// Reports an array of distinct strings.
fn is_names(value: &Value) -> bool {
    let Some(items) = value.as_array() else {
        return false;
    };
    let mut seen = HashSet::with_capacity(items.len());
    items
        .iter()
        .all(|item| item.as_str().is_some_and(|name| seen.insert(name)))
}

/// Reports a plain-name anchor: `^[A-Za-z_][-A-Za-z0-9._]*$`.
fn is_anchor_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_'))
}

/// Decodes the `%XX` escapes of a URI fragment, as Go's `url.PathUnescape`
/// does: `None` for a `%` that two hex digits do not follow.
fn percent_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = |at: usize| {
                bytes
                    .get(at)
                    .and_then(|byte| char::from(*byte).to_digit(16))
            };
            let (high, low) = (hex(i + 1)?, hex(i + 2)?);
            out.push(u8::try_from(high * 16 + low).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(out)
}

/// Reads an RFC 6901 array index below `len`, as Podium does: no leading zero,
/// and otherwise what Go's `strconv.Atoi` reads, a sign included.
fn array_index(token: &str, len: usize) -> Option<usize> {
    if token.is_empty() || (token.len() > 1 && token.starts_with('0')) {
        return None;
    }
    let index = token.parse::<i64>().ok()?;
    usize::try_from(index).ok().filter(|index| *index < len)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::{
        WORKLOAD_SCHEMA_DIALECT, WorkloadSchema, WorkloadSchemaError,
        WorkloadSchemaErrorReason as Reason,
    };

    fn accept(schema: Value) {
        if let Err(err) = WorkloadSchema::from_value(&schema) {
            panic!("{schema} must be accepted: {err}");
        }
    }

    fn refuse(schema: Value) -> WorkloadSchemaError {
        match WorkloadSchema::from_value(&schema) {
            Ok(_) => panic!("{schema} must be refused"),
            Err(err) => err,
        }
    }

    #[test]
    fn each_refused_keyword_is_refused_with_its_location() {
        for (schema, keyword, location) in [
            (
                json!({"properties": {"a": {"$dynamicRef": "#n"}}}),
                "$dynamicRef",
                "#/properties/a",
            ),
            (json!({"$dynamicAnchor": "n"}), "$dynamicAnchor", "#"),
            (
                json!({"allOf": [{"$recursiveRef": "#"}]}),
                "$recursiveRef",
                "#/allOf/0",
            ),
            (json!({"$recursiveAnchor": true}), "$recursiveAnchor", "#"),
            (
                json!({"$vocabulary": {"https://json-schema.org/draft/2020-12/vocab/core": true}}),
                "$vocabulary",
                "#",
            ),
            (
                json!({"items": {"unevaluatedItems": false}}),
                "unevaluatedItems",
                "#/items",
            ),
            (
                json!({"$defs": {"a": {"unevaluatedProperties": false}}}),
                "unevaluatedProperties",
                "#/$defs/a",
            ),
        ] {
            let err = refuse(schema);
            assert_eq!(err.location, location, "{err}");
            assert!(
                matches!(err.reason, Reason::UnsupportedKeyword { keyword: k, .. } if k == keyword),
                "{err}"
            );
        }
    }

    #[test]
    fn dialect_is_2020_12_or_absent() {
        assert_eq!(
            WORKLOAD_SCHEMA_DIALECT,
            "https://json-schema.org/draft/2020-12/schema"
        );
        accept(json!({"$schema": WORKLOAD_SCHEMA_DIALECT, "type": "object"}));
        accept(json!({"type": "object"}));
        // Podium accepts the exact URI below the root as well.
        accept(json!({"properties": {"a": {"$schema": WORKLOAD_SCHEMA_DIALECT}}}));

        for (schema, location) in [
            (
                json!({"$schema": "http://json-schema.org/draft-07/schema#"}),
                "#",
            ),
            (
                json!({"$schema": "https://json-schema.org/draft/2019-09/schema"}),
                "#",
            ),
            (
                json!({"$schema": "https://json-schema.org/draft/2020-12/schema#"}),
                "#",
            ),
            (json!({"$schema": 7}), "#"),
            (
                json!({"properties": {"a": {"$schema": "http://json-schema.org/draft-07/schema#"}}}),
                "#/properties/a",
            ),
        ] {
            let err = refuse(schema);
            assert_eq!(err.location, location, "{err}");
            assert!(
                matches!(err.reason, Reason::UnsupportedDialect { .. }),
                "{err}"
            );
        }
    }

    #[test]
    fn id_is_accepted_only_at_the_root() {
        accept(json!({"$id": "https://example.com/schemas/deploy.json", "type": "object"}));

        let err = refuse(json!({"$defs": {"a": {"$id": "a.json", "type": "string"}}}));
        assert_eq!(err.location, "#/$defs/a");
        assert_eq!(err.reason, Reason::NestedId);

        // A `$ref` target outside the tree of subschemas is not the root either.
        let err = refuse(json!({"x-defs": {"a": {"$id": "a.json"}}, "$ref": "#/x-defs/a"}));
        assert_eq!(err.location, "#/x-defs/a");
        assert_eq!(err.reason, Reason::NestedId);

        let err = refuse(json!({"$id": 5}));
        assert!(
            matches!(
                err.reason,
                Reason::InvalidKeywordValue { keyword: "$id", .. }
            ),
            "{err}"
        );
    }

    #[test]
    fn refs_stay_inside_the_document() {
        for reference in [
            "https://example.com/s",
            "file:///etc/passwd",
            "other.json#/a",
            "",
        ] {
            let err = refuse(json!({"$ref": reference}));
            assert_eq!(err.location, "#");
            assert_eq!(
                err.reason,
                Reason::RemoteReference {
                    reference: reference.to_owned()
                }
            );
        }

        let err = refuse(json!({"properties": {"a": {"$ref": "#/$defs/missing"}}}));
        assert_eq!(err.location, "#/properties/a");
        assert_eq!(
            err.reason,
            Reason::UnresolvedReference {
                reference: "#/$defs/missing".to_owned()
            }
        );

        accept(
            json!({"$defs": {"a": {"type": "string"}}, "properties": {"x": {"$ref": "#/$defs/a"}}}),
        );
        accept(json!({
            "$defs": {"a": {"$anchor": "anchor", "type": "string"}},
            "properties": {"x": {"$ref": "#anchor"}}
        }));
        accept(json!({"properties": {"next": {"$ref": "#"}}}));
        // RFC 6901 escapes, and percent-encoding as a URI fragment has it.
        accept(json!({
            "$defs": {"a/b": true, "c~d": true},
            "properties": {
                "x": {"$ref": "#/$defs/a~1b"},
                "y": {"$ref": "#/$defs/c~0d"},
                "z": {"$ref": "#/%24defs/a~1b"}
            }
        }));
        accept(json!({
            "prefixItems": [true, {"type": "string"}],
            "properties": {"x": {"$ref": "#/prefixItems/1"}}
        }));

        for (schema, reference) in [
            (
                json!({"$defs": {"a": true}, "$ref": "#/$defs/a~2"}),
                "#/$defs/a~2",
            ),
            (json!({"$ref": "#/%zz"}), "#/%zz"),
            (
                json!({"x-defs": {"n": 5}, "$ref": "#/x-defs/n"}),
                "#/x-defs/n",
            ),
            (json!({"$ref": "#nowhere"}), "#nowhere"),
            (json!({"$ref": "#not an anchor"}), "#not an anchor"),
            (
                json!({"prefixItems": [true], "$ref": "#/prefixItems/01"}),
                "#/prefixItems/01",
            ),
        ] {
            let err = refuse(schema);
            assert_eq!(
                err.reason,
                Reason::UnresolvedReference {
                    reference: reference.to_owned()
                },
                "{err}"
            );
        }

        let err = refuse(json!({"$ref": 5}));
        assert!(
            matches!(
                err.reason,
                Reason::InvalidKeywordValue {
                    keyword: "$ref",
                    ..
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn same_location_cycles_are_refused_recursion_through_properties_is_not() {
        for schema in [
            json!({"$ref": "#"}),
            json!({"$defs": {"a": {"$ref": "#/$defs/b"}, "b": {"$ref": "#/$defs/a"}}, "$ref": "#/$defs/a"}),
            json!({"$defs": {"a": {"anyOf": [{"type": "string"}, {"$ref": "#/$defs/a"}]}}, "$ref": "#/$defs/a"}),
            json!({"oneOf": [{"$ref": "#"}]}),
            json!({"$defs": {"a": {"if": true, "then": {"$ref": "#/$defs/a"}}}, "$ref": "#/$defs/a"}),
            json!({"if": false, "else": {"$ref": "#"}}),
            json!({"dependentSchemas": {"x": {"$ref": "#"}}}),
            json!({"$defs": {"a": {"not": {"allOf": [{"$ref": "#/$defs/a"}]}}}, "$ref": "#/$defs/a"}),
            json!({"$defs": {"a": {"$anchor": "a", "allOf": [{"$ref": "#a"}]}}}),
        ] {
            let err = refuse(schema);
            assert_eq!(err.reason, Reason::ReferenceCycle, "{err}");
        }
        assert_eq!(refuse(json!({"$ref": "#"})).location, "#");
        let err = refuse(json!({
            "$defs": {"a": {"$ref": "#/$defs/b"}, "b": {"$ref": "#/$defs/a"}},
            "$ref": "#/$defs/a"
        }));
        assert_eq!(err.location, "#/$defs/a");

        for schema in [
            json!({"properties": {"next": {"$ref": "#"}}}),
            json!({"properties": {"children": {"items": {"$ref": "#"}}}}),
            json!({"anyOf": [
                {"type": ["null", "boolean", "number", "string"]},
                {"type": "array", "items": {"$ref": "#"}},
                {"type": "object", "additionalProperties": {"$ref": "#"}}
            ]}),
            json!({"propertyNames": {"$ref": "#"}}),
            json!({"contains": {"$ref": "#"}}),
            json!({"then": {"$ref": "#"}}),
        ] {
            accept(schema);
        }
    }

    #[test]
    fn keyword_values_are_type_checked() {
        for (schema, keyword) in [
            (json!({"type": "int"}), "type"),
            (json!({"type": ["string", "string"]}), "type"),
            (json!({"type": []}), "type"),
            (json!({"type": 5}), "type"),
            (json!({"required": ["a", "a"]}), "required"),
            (json!({"required": [1]}), "required"),
            (json!({"minLength": -1}), "minLength"),
            (json!({"minLength": 1.5}), "minLength"),
            (json!({"maxItems": "1"}), "maxItems"),
            (json!({"multipleOf": 0}), "multipleOf"),
            (json!({"multipleOf": -2}), "multipleOf"),
            (json!({"maximum": true}), "maximum"),
            (json!({"allOf": []}), "allOf"),
            (json!({"anyOf": {}}), "anyOf"),
            (json!({"items": [{"type": "string"}]}), "items"),
            (json!({"not": 5}), "not"),
            (json!({"examples": {}}), "examples"),
            (json!({"deprecated": "yes"}), "deprecated"),
            (json!({"title": 5}), "title"),
            (json!({"enum": "a"}), "enum"),
            (json!({"pattern": 5}), "pattern"),
            (
                json!({"dependentRequired": {"a": "b"}}),
                "dependentRequired",
            ),
            (json!({"properties": []}), "properties"),
            (json!({"contentSchema": 5}), "contentSchema"),
            (json!({"uniqueItems": "yes"}), "uniqueItems"),
            (json!({"$defs": 5}), "$defs"),
        ] {
            let err = refuse(schema.clone());
            assert_eq!(err.location, "#", "{schema}: {err}");
            assert!(
                matches!(err.reason, Reason::InvalidKeywordValue { keyword: k, .. } if k == keyword),
                "{schema}: {err}"
            );
        }

        // A member or an item that is not a schema is refused where it is.
        for (schema, keyword, location) in [
            (
                json!({"properties": {"a": "string"}}),
                "properties",
                "#/properties/a",
            ),
            (json!({"$defs": {"n": 5}}), "$defs", "#/$defs/n"),
            (json!({"allOf": [true, 5]}), "allOf", "#/allOf/1"),
        ] {
            let err = refuse(schema);
            assert_eq!(err.location, location, "{err}");
            assert!(
                matches!(err.reason, Reason::InvalidKeywordValue { keyword: k, .. } if k == keyword),
                "{err}"
            );
        }

        // A count may be written as any integral number, as its canonical form is.
        accept(json!({
            "minLength": 1.0,
            "maxLength": 1e2,
            "minItems": 0,
            "maxProperties": -0.0,
            "multipleOf": 0.5,
            "maximum": -3
        }));
    }

    #[test]
    fn unknown_and_x_keywords_are_ignored_unless_referenced() {
        accept(json!({"x-defs": {"a": {"$dynamicRef": "#x"}}}));

        let err = refuse(json!({"x-defs": {"a": {"$dynamicRef": "#x"}}, "$ref": "#/x-defs/a"}));
        assert_eq!(err.location, "#/x-defs/a");
        assert!(
            matches!(
                err.reason,
                Reason::UnsupportedKeyword {
                    keyword: "$dynamicRef",
                    ..
                }
            ),
            "{err}"
        );

        // Values that are not evaluated are not compiled, whatever they hold.
        for schema in [
            json!({"foo": {"unevaluatedItems": false}}),
            json!({"default": {"$dynamicRef": "#n", "$id": "x"}}),
            json!({"examples": [{"$ref": "https://example.com/s.json"}]}),
            json!({"contentSchema": {"unevaluatedProperties": false}}),
            json!({"enum": [{"$id": "x", "$schema": "y"}]}),
            json!({"const": {"pattern": "(?=a)"}}),
            json!({"x-pattern": {"pattern": "(?=a)"}}),
        ] {
            accept(schema);
        }
    }

    #[test]
    fn anchors_are_plain_names_and_unique() {
        accept(json!({"$anchor": "_a.b-c9", "$defs": {"x": {"$anchor": "X"}}}));

        for name in ["#a", "1a", "", "a b", "a/b", "é"] {
            let err = refuse(json!({"$anchor": name}));
            assert!(
                matches!(
                    err.reason,
                    Reason::InvalidKeywordValue {
                        keyword: "$anchor",
                        ..
                    }
                ),
                "{name:?}: {err}"
            );
        }
        assert!(matches!(
            refuse(json!({"$anchor": 5})).reason,
            Reason::InvalidKeywordValue {
                keyword: "$anchor",
                ..
            }
        ));

        let err = refuse(json!({"$defs": {"a": {"$anchor": "x"}, "b": {"$anchor": "x"}}}));
        assert_eq!(err.location, "#/$defs/b");
        assert_eq!(
            err.reason,
            Reason::DuplicateAnchor {
                anchor: "x".to_owned()
            }
        );

        // Only the tree of subschemas declares anchors, as in Podium: one under
        // a keyword the subset does not know declares nothing.
        let err = refuse(json!({"x-defs": {"a": {"$anchor": "a"}}, "$ref": "#a"}));
        assert_eq!(
            err.reason,
            Reason::UnresolvedReference {
                reference: "#a".to_owned()
            }
        );
        accept(json!({"x-defs": {"a": {"$anchor": "a"}}, "$ref": "#/x-defs/a"}));
    }

    #[test]
    fn patterns_are_checked_where_the_walk_reaches_them() {
        let err = refuse(json!({"patternProperties": {"(?=a)": true}}));
        assert_eq!(err.location, "#");
        assert!(matches!(err.reason, Reason::InvalidRegex { .. }), "{err}");

        let err = refuse(json!({"x": {"pattern": "(?=a)"}, "$ref": "#/x"}));
        assert_eq!(err.location, "#/x");
        assert!(matches!(err.reason, Reason::InvalidRegex { .. }), "{err}");
    }
}
