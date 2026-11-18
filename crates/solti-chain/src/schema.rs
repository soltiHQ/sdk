//! JSON Schema refinements for the chain model.

use schemars::{Schema, SchemaGenerator, json_schema};

/// Restricts the shared workload schema to routed, non-chain workloads.
pub(crate) fn chain_step_workload(generator: &mut SchemaGenerator) -> Schema {
    let workload = generator.subschema_for::<solti_model::Workload>();
    json_schema!({
        "allOf": [
            workload,
            {
                "not": {
                    "type": "object",
                    "required": ["apiVersion", "kind"],
                    "properties": {
                        "apiVersion": { "const": crate::CHAIN_API_VERSION },
                        "kind": { "const": crate::CHAIN_KIND }
                    }
                }
            }
        ]
    })
}
