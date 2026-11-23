//! Schema generation from Rust types.
//!
//! The schema a Rust body type derives is shaped for Podium: draft 2020-12 with
//! `unevaluatedProperties` rewritten (Podium does not accept it) and 64-bit integers bounded to
//! the range a JSON number carries exactly.

use schemars::{
    Schema, SchemaGenerator,
    generate::SchemaSettings,
    transform::{ReplaceUnevaluatedProperties, Transform, transform_subschemas},
};
use serde_json::{Number, Value};

use super::decl::MAX_JSON_SAFE_INTEGER;

/// Formats whose integers may exceed [`MAX_JSON_SAFE_INTEGER`], signed first.
const SIGNED_FORMATS: [&str; 3] = ["int64", "int", "int128"];
const UNSIGNED_FORMATS: [&str; 3] = ["uint64", "uint", "uint128"];

/// Derives the draft 2020-12 schema of `T`, ready for [`WorkloadSchema::from_value`](crate::WorkloadSchema::from_value).
///
/// The generator runs [`ReplaceUnevaluatedProperties`], then [`BoundJsonSafeIntegers`].
pub fn workload_schema_for<T: schemars::JsonSchema>() -> Value {
    let settings = SchemaSettings::draft2020_12()
        .with_transform(ReplaceUnevaluatedProperties::default())
        .with_transform(BoundJsonSafeIntegers);
    SchemaGenerator::new(settings)
        .into_root_schema_for::<T>()
        .to_value()
}

/// Bounds every 64-bit-or-wider integer subschema to `[-MAX_JSON_SAFE_INTEGER, MAX_JSON_SAFE_INTEGER]`
/// (`[0, MAX_JSON_SAFE_INTEGER]` for the unsigned formats), keeping a tighter bound that is already set.
///
/// A subschema qualifies when its `type` is or includes `"integer"` and its `format` is one of
/// `int64`, `uint64`, `int`, `uint`, `int128`, `uint128`.
#[derive(Clone, Copy, Debug, Default)]
pub struct BoundJsonSafeIntegers;

impl Transform for BoundJsonSafeIntegers {
    fn transform(&mut self, schema: &mut Schema) {
        if let Some(object) = schema.as_object_mut() {
            let is_integer = match object.get("type") {
                Some(Value::String(name)) => name == "integer",
                Some(Value::Array(names)) => names.iter().any(|name| name == "integer"),
                _ => false,
            };
            let format = object.get("format").and_then(Value::as_str);
            let signed = format.is_some_and(|f| SIGNED_FORMATS.contains(&f));
            let unsigned = format.is_some_and(|f| UNSIGNED_FORMATS.contains(&f));
            if is_integer && (signed || unsigned) {
                let maximum = tighter(object.get("maximum"), MAX_JSON_SAFE_INTEGER, Side::Max);
                let floor = if signed { -MAX_JSON_SAFE_INTEGER } else { 0 };
                let minimum = tighter(object.get("minimum"), floor, Side::Min);
                object.insert("maximum".to_owned(), maximum);
                object.insert("minimum".to_owned(), minimum);
            }
        }
        transform_subschemas(self, schema);
    }
}

#[derive(Clone, Copy)]
enum Side {
    Min,
    Max,
}

/// The existing bound when it is inside `limit`, otherwise `limit`.
fn tighter(existing: Option<&Value>, limit: i64, side: Side) -> Value {
    let limit_value = Value::from(limit);
    let Some(Value::Number(number)) = existing else {
        return limit_value;
    };
    if inside(number, limit, side) {
        Value::Number(number.clone())
    } else {
        limit_value
    }
}

fn inside(number: &Number, limit: i64, side: Side) -> bool {
    if let Some(value) = number.as_i64() {
        return match side {
            Side::Max => value <= limit,
            Side::Min => value >= limit,
        };
    }
    if number.as_u64().is_some() {
        // Above i64::MAX: past any upper limit, never below a lower one.
        return matches!(side, Side::Min);
    }
    number.as_f64().is_some_and(|value| match side {
        Side::Max => value <= limit as f64,
        Side::Min => value >= limit as f64,
    })
}
