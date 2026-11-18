//! Cross-language vectors: Podium's own Go code is the oracle.
//!
//! `tests/data/podium/in` holds raw inputs: JCS documents, schemas and regexes.
//! Podium's `internal/workloadschema` read them, at the commit that
//! `out/generator.json` records, and wrote its verdicts to
//! `tests/data/podium/out`, which is never edited by hand. The SDK must agree:
//!
//! - **JCS:** the canonical bytes and the fingerprint of every input Go
//!   accepts, and a refusal of every input Go refuses, but for duplicate keys,
//!   which a `serde_json::Value` cannot hold: it keeps the last one.
//! - **Schemas:** never accepted by the SDK when Podium refuses them. A schema
//!   of `in/compile` is named for the verdicts: both accept `ok-*`, both refuse
//!   `bad-*`, and the SDK refuses `sdk-strict-*` on purpose, which Podium
//!   accepts.
//!
//! The regex vectors are checked against the SDK's regex estimate, next to it
//! in `src/domain/workload/schema/regex.rs`.
//!
//! The verdict tests check every vector, then list each one that diverges.

use std::{fs, path::Path};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use solti_model::{WorkloadSchema, canonical_json};

const JCS: &str = include_str!("data/podium/out/jcs.json");
const COMPILE: &str = include_str!("data/podium/out/compile.json");
const REGEX: &str = include_str!("data/podium/out/regex.json");

fn vectors(text: &str) -> Vec<Value> {
    serde_json::from_str(text).expect("the vectors are JSON")
}

fn text<'a>(vector: &'a Value, field: &str) -> &'a str {
    vector[field]
        .as_str()
        .unwrap_or_else(|| panic!("`{field}` is a string in {vector}"))
}

/// A base64 field of a vector, decoded; `None` when the field is absent.
fn decoded(vector: &Value, field: &str) -> Option<Vec<u8>> {
    vector[field]
        .as_str()
        .map(|encoded| BASE64.decode(encoded).expect("the field is base64"))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Fails with every divergence found, one per line.
fn assert_none(failures: &[String], checked: usize) {
    assert!(
        failures.is_empty(),
        "{} of {checked} vectors diverge from Podium:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn jcs_and_fingerprints_match_podium() {
    let vectors = vectors(JCS);
    assert!(vectors.len() >= 40, "{} JCS vectors", vectors.len());
    let mut failures = Vec::new();
    for vector in &vectors {
        let name = text(vector, "name");
        let input = decoded(vector, "input").expect("every vector has its input");
        let go = decoded(vector, "canonical");
        if name.starts_with("dupkey-") {
            if go.is_some() {
                failures.push(format!("{name}: Go accepts a duplicate key"));
            }
            continue;
        }

        let sdk = serde_json::from_slice::<Value>(&input)
            .map_err(|err| err.to_string())
            .and_then(|value| {
                let canonical = canonical_json(&value).map_err(|err| err.to_string())?;
                Ok((value, canonical))
            });
        match (go, sdk) {
            (None, Err(_)) => {}
            (None, Ok(_)) => failures.push(format!(
                "{name}: Go refuses it ({}), the SDK accepts it",
                text(vector, "error")
            )),
            (Some(_), Err(err)) => {
                failures.push(format!("{name}: Go accepts it, the SDK refuses it: {err}"));
            }
            (Some(go), Ok((value, sdk))) => {
                if sdk != go {
                    failures.push(format!(
                        "{name}: the SDK writes {:?}, Go writes {:?}",
                        lossy(&sdk),
                        lossy(&go)
                    ));
                } else if value.is_object()
                    && hex(&Sha256::digest(&sdk)) != text(vector, "fingerprint")
                {
                    failures.push(format!("{name}: the fingerprints differ"));
                }
            }
        }
    }
    assert_none(&failures, vectors.len());
}

#[test]
fn subset_verdicts_match_podium() {
    let vectors = vectors(COMPILE);
    // ok-*, bad-* and sdk-strict-* inputs.
    let mut labels = [0_usize; 3];
    let mut failures = Vec::new();
    for vector in &vectors {
        let name = text(vector, "name");
        let raw = decoded(vector, "schema").expect("every vector has its schema");
        let podium_ok = vector["ok"].as_bool().expect("`ok` is a boolean");
        let podium_error = vector["error"].as_str().unwrap_or_default();

        // What the name says each side does. A schema the SDK emits has no
        // label: the SDK must accept it whenever Podium does.
        let (podium_must, sdk_must) = match name.strip_prefix("compile/") {
            None => (podium_ok, podium_ok),
            Some(file) if file.starts_with("ok-") => {
                labels[0] += 1;
                (true, true)
            }
            Some(file) if file.starts_with("bad-") => {
                labels[1] += 1;
                (false, false)
            }
            Some(file) if file.starts_with("sdk-strict-") => {
                labels[2] += 1;
                (true, false)
            }
            Some(file) => {
                failures.push(format!(
                    "compile/{file}: name it ok-*, bad-* or sdk-strict-*"
                ));
                continue;
            }
        };
        if podium_ok != podium_must {
            failures.push(format!(
                "{name}: Podium's verdict is not what the name says: {}",
                if podium_ok {
                    "it accepts it"
                } else {
                    podium_error
                }
            ));
        }

        let sdk = serde_json::from_slice::<Value>(&raw)
            .map_err(|err| err.to_string())
            .and_then(|value| WorkloadSchema::from_value(&value).map_err(|err| err.to_string()));
        match sdk {
            Ok(_) if !podium_ok => failures.push(format!(
                "{name}: the SDK accepts a schema Podium refuses: {podium_error}"
            )),
            Ok(schema) if schema.fingerprint().as_str() != text(vector, "fingerprint") => {
                failures.push(format!("{name}: the fingerprints differ"));
            }
            Ok(_) if !sdk_must => {
                failures.push(format!(
                    "{name}: the SDK accepts it, its name says it refuses it"
                ));
            }
            Err(err) if sdk_must => {
                failures.push(format!(
                    "{name}: Podium accepts it, the SDK refuses it: {err}"
                ));
            }
            Ok(_) | Err(_) => {}
        }
    }
    assert!(
        labels.iter().all(|count| *count > 0),
        "ok, bad and sdk-strict vectors: {labels:?}"
    );
    assert_none(&failures, vectors.len());
}

/// The vectors in `out/` come from exactly the inputs in `in/`.
#[test]
fn vectors_are_generated_from_the_inputs() {
    let inputs = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/podium/in");
    for (dir, prefix, vectors, field) in [
        ("jcs", "", vectors(JCS), "input"),
        ("compile", "compile/", vectors(COMPILE), "schema"),
    ] {
        // In the generator's order: sorted by file name, `.json` included.
        let mut paths: Vec<_> = fs::read_dir(inputs.join(dir))
            .expect("the input directory exists")
            .map(|entry| entry.expect("a readable entry").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        paths.sort();
        let files: Vec<(String, Vec<u8>)> = paths
            .iter()
            .map(|path| {
                let stem = path.file_stem().expect("a file name").to_string_lossy();
                (
                    format!("{prefix}{stem}"),
                    fs::read(path).expect("a readable input"),
                )
            })
            .collect();
        let generated: Vec<(String, Vec<u8>)> = vectors
            .iter()
            .map(|vector| {
                let raw = decoded(vector, field).expect("every vector has its input");
                (text(vector, "name").to_owned(), raw)
            })
            .filter(|(name, _)| name.starts_with(prefix))
            .collect();

        let names = |list: &[(String, Vec<u8>)]| {
            list.iter()
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(&generated),
            names(&files),
            "in/{dir}: regenerate the vectors"
        );
        for ((name, generated), (_, file)) in generated.iter().zip(&files) {
            assert!(
                generated == file,
                "{name}: the input changed since the vectors were generated"
            );
        }
    }

    let patterns: Vec<String> =
        serde_json::from_slice(&fs::read(inputs.join("regex.json")).expect("in/regex.json exists"))
            .expect("in/regex.json is an array of strings");
    let regex = vectors(REGEX);
    let generated: Vec<&str> = regex.iter().map(|vector| text(vector, "pattern")).collect();
    assert_eq!(generated, patterns, "in/regex.json: regenerate the vectors");
}
