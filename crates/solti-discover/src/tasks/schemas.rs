//! # Workload schema exchange
//!
//! Podium names each declared schema by its fingerprint and asks, in
//! `want_schemas`, for the ones it lacks. [`SchemaExchange`] decides which
//! schemas a follow-up request carries and reads what each accepted response
//! asks for. The heartbeat never carries schemas. The exchange does no I/O:
//! the caller measures requests with the transport's encoder, sends them, and
//! logs what [`Observation`] reports.
//!
//! ```text
//! accepted response ── want_schemas ──► observe ──► wanted, follow_up
//!                                                       │
//! next follow-up ◄── schemas, sorted, within the budget ── attach
//! ```
//!
//! A wanted schema is *due* when it was never sent, or was last sent at least
//! [`SCHEMA_RESEND_INTERVAL`] ago, and it is *sendable* when it is due and the
//! current attempt has not sent it yet. An attempt sends each schema at most
//! once, however long its follow-ups take.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::{Duration, Instant};

use solti_model::{AgentCapabilities, SchemaFingerprint, WorkloadSchema, WorkloadTypeMeta};

use crate::proto::{SyncRequest, WorkloadSchema as ProtoWorkloadSchema};

/// Most bytes one encoded discovery request may take: Podium's cap on an
/// HTTP body or a gRPC message.
pub(super) const MAX_SYNC_REQUEST_BYTES: usize = 256 * 1024;

/// Bytes the packing of schemas leaves free below [`MAX_SYNC_REQUEST_BYTES`].
pub(super) const SYNC_REQUEST_HEADROOM_BYTES: usize = 1024;

/// Most encoded bytes a request may take once its schemas are attached.
pub(super) const SYNC_REQUEST_BUDGET: usize = MAX_SYNC_REQUEST_BYTES - SYNC_REQUEST_HEADROOM_BYTES;

/// Most follow-up requests one attempt sends after its heartbeat.
pub(super) const MAX_SCHEMA_FOLLOW_UPS: usize = 6;

/// Least time between two sends of one schema that Podium still wants.
pub(super) const SCHEMA_RESEND_INTERVAL: Duration = Duration::from_secs(300);

/// What one accepted response asks for.
#[derive(Debug)]
pub(super) struct Observation {
    /// Fingerprints the response wants although the same request carried them:
    /// a store error or a fingerprint mismatch on Podium's side. Each is
    /// reported once in the life of the exchange.
    pub(super) still_requested: Vec<SchemaFingerprint>,
    /// Wanted entries the agent does not declare, as received; they are
    /// ignored.
    pub(super) unknown: Vec<String>,
    /// Whether a wanted schema is sendable now: due, and not sent in this
    /// attempt.
    pub(super) follow_up: bool,
}

/// One declared schema and the workload types that declare it.
struct DeclaredSchema {
    schema: WorkloadSchema,
    /// Each GVK once, in first-declaration order.
    types: Vec<WorkloadTypeMeta>,
}

impl DeclaredSchema {
    fn to_proto(&self) -> ProtoWorkloadSchema {
        ProtoWorkloadSchema {
            fingerprint: self.schema.fingerprint().as_str().to_owned(),
            schema: self.schema.canonical_json().to_vec(),
        }
    }
}

/// The agent's side of the schema exchange, kept across attempts.
pub(super) struct SchemaExchange {
    /// Declared schemas by fingerprint.
    schemas: BTreeMap<SchemaFingerprint, DeclaredSchema>,
    /// Declared fingerprints the last accepted response wanted, sorted as
    /// Podium sorts them.
    wanted: BTreeSet<SchemaFingerprint>,
    /// When each schema was last carried by an accepted request.
    last_sent: HashMap<SchemaFingerprint, Instant>,
    /// Fingerprints already reported as still requested.
    warned: HashSet<SchemaFingerprint>,
}

impl SchemaExchange {
    /// Starts an exchange for the schemas of `capabilities`; nothing is wanted
    /// until an accepted response asks.
    pub(super) fn new(capabilities: &AgentCapabilities) -> Self {
        let schemas = capabilities
            .schemas()
            .into_iter()
            .map(|schema| {
                let mut seen = HashSet::new();
                let types = capabilities
                    .types_for(schema.fingerprint())
                    .into_iter()
                    .filter(|meta| seen.insert(*meta))
                    .cloned()
                    .collect();
                (
                    schema.fingerprint().clone(),
                    DeclaredSchema {
                        schema: schema.clone(),
                        types,
                    },
                )
            })
            .collect();
        Self {
            schemas,
            wanted: BTreeSet::new(),
            last_sent: HashMap::new(),
            warned: HashSet::new(),
        }
    }

    /// Adds sendable schemas to `request`, in sorted order, while `measure`
    /// keeps the request within [`SYNC_REQUEST_BUDGET`], and returns their
    /// fingerprints.
    ///
    /// The first schema that does not fit ends the packing; the rest go in a
    /// later request. A schema that does not fit even beside the bare request
    /// can never be sent, so it is skipped rather than allowed to hold back the
    /// others.
    pub(super) fn attach(
        &self,
        request: &mut SyncRequest,
        now: Instant,
        measure: impl Fn(&SyncRequest) -> usize,
        sent_this_attempt: &HashSet<SchemaFingerprint>,
    ) -> Vec<SchemaFingerprint> {
        let mut attached = Vec::new();
        for (fingerprint, declared) in self.sendable(now, sent_this_attempt) {
            request.schemas.push(declared.to_proto());
            if measure(request) <= SYNC_REQUEST_BUDGET {
                attached.push(fingerprint.clone());
                continue;
            }
            request.schemas.pop();
            if !request.schemas.is_empty() {
                break;
            }
        }
        attached
    }

    /// Takes in an accepted response to a request that carried `sent`.
    ///
    /// It records `sent` as sent at `now` and adds it to `sent_this_attempt`,
    /// replaces the wanted set with the declared fingerprints of `want`, and
    /// reports what [`Observation`] describes.
    pub(super) fn observe(
        &mut self,
        sent: &[SchemaFingerprint],
        want: &[String],
        now: Instant,
        sent_this_attempt: &mut HashSet<SchemaFingerprint>,
    ) -> Observation {
        for fingerprint in sent {
            self.last_sent.insert(fingerprint.clone(), now);
            sent_this_attempt.insert(fingerprint.clone());
        }

        self.wanted.clear();
        let mut unknown = Vec::new();
        for entry in want {
            match SchemaFingerprint::parse(entry) {
                Ok(fingerprint) if self.schemas.contains_key(&fingerprint) => {
                    self.wanted.insert(fingerprint);
                }
                _ => unknown.push(entry.clone()),
            }
        }

        let still_requested = sent
            .iter()
            .filter(|fingerprint| self.wanted.contains(*fingerprint))
            .filter(|fingerprint| self.warned.insert((*fingerprint).clone()))
            .cloned()
            .collect();
        let follow_up = self.sendable(now, sent_this_attempt).next().is_some();
        Observation {
            still_requested,
            unknown,
            follow_up,
        }
    }

    /// Declared schemas that do not fit beside `bare` within the budget, in
    /// sorted order: they can never be sent.
    pub(super) fn unsendable(
        &self,
        bare: &SyncRequest,
        measure: impl Fn(&SyncRequest) -> usize,
    ) -> Vec<SchemaFingerprint> {
        let mut request = bare.clone();
        let mut unsendable = Vec::new();
        for (fingerprint, declared) in &self.schemas {
            request.schemas.push(declared.to_proto());
            if measure(&request) > SYNC_REQUEST_BUDGET {
                unsendable.push(fingerprint.clone());
            }
            request.schemas.pop();
        }
        unsendable
    }

    /// The `apiVersion/kind` of each workload type with this schema, joined by
    /// commas; empty for an undeclared fingerprint.
    pub(super) fn gvks(&self, fingerprint: &SchemaFingerprint) -> String {
        self.schemas
            .get(fingerprint)
            .map(|declared| {
                declared
                    .types
                    .iter()
                    .map(|meta| format!("{}/{}", meta.api_version(), meta.kind()))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    }

    /// The canonical size of this schema; zero for an undeclared fingerprint.
    pub(super) fn schema_bytes(&self, fingerprint: &SchemaFingerprint) -> usize {
        self.schemas
            .get(fingerprint)
            .map_or(0, |declared| declared.schema.canonical_json().len())
    }

    /// Wanted schemas that are due at `now` and not sent in this attempt, in
    /// sorted order.
    fn sendable<'a>(
        &'a self,
        now: Instant,
        sent_this_attempt: &'a HashSet<SchemaFingerprint>,
    ) -> impl Iterator<Item = (&'a SchemaFingerprint, &'a DeclaredSchema)> + 'a {
        self.wanted
            .iter()
            .filter(move |fingerprint| !sent_this_attempt.contains(*fingerprint))
            .filter(move |fingerprint| {
                self.last_sent.get(*fingerprint).is_none_or(|sent| {
                    now.saturating_duration_since(*sent) >= SCHEMA_RESEND_INTERVAL
                })
            })
            .filter_map(|fingerprint| {
                self.schemas
                    .get(fingerprint)
                    .map(|declared| (fingerprint, declared))
            })
    }
}

/// Test data shared by the discovery tests: schemas of an exact size and the
/// capability snapshots that declare them.
#[cfg(test)]
pub(super) mod fixtures {
    use serde_json::json;
    use solti_model::{
        AgentCapabilities, Labels, RunnerCapability, WorkloadSchema, WorkloadType,
        WorkloadTypeMeta, canonical_json,
    };

    /// A schema whose canonical JSON takes exactly `len` bytes: a string `enum`
    /// padded to size, named by `tag` so that each tag has its own fingerprint.
    pub(in crate::tasks) fn sized_schema(tag: &str, len: usize) -> WorkloadSchema {
        let shaped = |pad: usize| json!({"type": "string", "enum": [tag, "x".repeat(pad)]});
        let bare = canonical_json(&shaped(0)).expect("canonical JSON").len();
        let schema = WorkloadSchema::from_value(&shaped(len - bare)).expect("schema in the subset");
        assert_eq!(schema.canonical_json().len(), len);
        schema
    }

    /// A small object schema named by `tag`.
    pub(in crate::tasks) fn small_schema(tag: &str) -> WorkloadSchema {
        WorkloadSchema::from_value(&json!({"type": "object", "description": tag}))
            .expect("schema in the subset")
    }

    /// One workload type.
    pub(in crate::tasks) fn workload(
        api_version: &str,
        kind: &str,
        schema: WorkloadSchema,
    ) -> WorkloadType {
        WorkloadType::new(
            WorkloadTypeMeta::new(api_version, kind).expect("valid GVK"),
            schema,
        )
    }

    /// One runner without labels.
    pub(in crate::tasks) fn runner(name: &str, workloads: Vec<WorkloadType>) -> RunnerCapability {
        RunnerCapability::new(name, Labels::new(), workloads).expect("valid runner")
    }

    /// A capability snapshot of `runners`, in registration order.
    pub(in crate::tasks) fn agent(runners: Vec<RunnerCapability>) -> AgentCapabilities {
        AgentCapabilities::new(runners).expect("valid capabilities")
    }

    /// One runner declaring `Big0`, `Big1`, ... : `count` types, each with its
    /// own schema of exactly `len` canonical bytes.
    pub(in crate::tasks) fn big_agent(count: usize, len: usize) -> AgentCapabilities {
        agent(vec![runner(
            "big",
            (0..count)
                .map(|i| {
                    workload(
                        "big.example.io/v1",
                        &format!("Big{i}"),
                        sized_schema(&format!("big-{i}"), len),
                    )
                })
                .collect(),
        )])
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::time::{Duration, Instant};

    use solti_model::SchemaFingerprint;

    use super::fixtures::{agent, big_agent, runner, sized_schema, small_schema, workload};
    use super::*;
    use crate::proto::{SyncRequest, WorkloadSchema as ProtoWorkloadSchema};

    /// Every declared fingerprint, as a control plane lists them in `want_schemas`.
    fn all_wanted(capabilities: &solti_model::AgentCapabilities) -> Vec<String> {
        capabilities
            .schemas()
            .iter()
            .map(|schema| schema.fingerprint().as_str().to_owned())
            .collect()
    }

    /// A stand-in for an encoder: a fixed base, and each schema as base64 in JSON.
    fn base64_measure(base: usize) -> impl Fn(&SyncRequest) -> usize {
        move |request| {
            base + request
                .schemas
                .iter()
                .map(|schema| 40 + schema.fingerprint.len() + schema.schema.len().div_ceil(3) * 4)
                .sum::<usize>()
        }
    }

    #[test]
    fn the_exchange_limits_are_podiums() {
        assert_eq!(MAX_SYNC_REQUEST_BYTES, 262_144);
        assert_eq!(SYNC_REQUEST_HEADROOM_BYTES, 1_024);
        assert_eq!(SYNC_REQUEST_BUDGET, 261_120);
        assert_eq!(MAX_SCHEMA_FOLLOW_UPS, 6);
        assert_eq!(SCHEMA_RESEND_INTERVAL, Duration::from_secs(300));
    }

    #[test]
    fn a_first_heartbeat_carries_no_schemas() {
        let capabilities = big_agent(3, 1_000);
        let exchange = SchemaExchange::new(&capabilities);
        let mut request = SyncRequest::default();

        let attached = exchange.attach(&mut request, Instant::now(), |_| 0, &HashSet::new());

        assert!(attached.is_empty());
        assert!(request.schemas.is_empty());
    }

    #[test]
    fn wanted_schemas_go_out_within_the_encoded_budget() {
        // Sizes that do not pack evenly: only the invariant holds for them (F2).
        let sizes = [
            40_000, 65_536, 10_000, 65_536, 63_000, 1_000, 30_000, 64_512,
        ];
        let capabilities = agent(vec![runner(
            "mixed",
            sizes
                .iter()
                .enumerate()
                .map(|(i, &len)| {
                    workload(
                        "mixed.example.io/v1",
                        &format!("Kind{i}"),
                        sized_schema(&format!("mixed-{i}"), len),
                    )
                })
                .collect(),
        )]);
        let canonical = |fingerprint: &str| {
            capabilities
                .schemas()
                .into_iter()
                .find(|schema| schema.fingerprint().as_str() == fingerprint)
                .map(|schema| schema.canonical_json().to_vec())
                .expect("declared schema")
        };
        let mut sorted: Vec<SchemaFingerprint> = capabilities
            .schemas()
            .iter()
            .map(|schema| schema.fingerprint().clone())
            .collect();
        sorted.sort();
        let measure = base64_measure(2_000);
        let now = Instant::now();
        let mut exchange = SchemaExchange::new(&capabilities);
        let mut sent_this_attempt = HashSet::new();
        let mut observation =
            exchange.observe(&[], &all_wanted(&capabilities), now, &mut sent_this_attempt);

        let mut sent_in_order: Vec<SchemaFingerprint> = Vec::new();
        let mut requests = 0;
        while observation.follow_up {
            requests += 1;
            assert!(requests <= sizes.len(), "every request carries a schema");
            let mut request = SyncRequest::default();
            let attached = exchange.attach(&mut request, now, &measure, &sent_this_attempt);

            assert!(!attached.is_empty());
            let size = measure(&request);
            assert!(size <= SYNC_REQUEST_BUDGET, "request of {size} bytes");
            assert_eq!(
                request
                    .schemas
                    .iter()
                    .map(|schema| (schema.fingerprint.clone(), schema.schema.clone()))
                    .collect::<Vec<_>>(),
                attached
                    .iter()
                    .map(|fingerprint| (
                        fingerprint.as_str().to_owned(),
                        canonical(fingerprint.as_str())
                    ))
                    .collect::<Vec<_>>(),
                "the canonical bytes go out under their fingerprints",
            );
            sent_in_order.extend(attached.iter().cloned());
            let remaining: Vec<&SchemaFingerprint> = sorted
                .iter()
                .filter(|fingerprint| !sent_in_order.contains(*fingerprint))
                .collect();
            if let Some(next) = remaining.first() {
                let mut one_more = request.clone();
                one_more.schemas.push(ProtoWorkloadSchema {
                    fingerprint: next.as_str().to_owned(),
                    schema: canonical(next.as_str()),
                });
                assert!(
                    measure(&one_more) > SYNC_REQUEST_BUDGET,
                    "a request carries as many schemas as fit"
                );
            }
            let want: Vec<String> = remaining
                .iter()
                .map(|fingerprint| fingerprint.as_str().to_owned())
                .collect();
            observation = exchange.observe(&attached, &want, now, &mut sent_this_attempt);
        }

        assert_eq!(sent_in_order, sorted, "every schema once, in sorted order");
        assert!(requests >= 2);
    }

    #[test]
    fn a_schema_is_not_resent_within_five_minutes_and_warns_once_when_still_requested() {
        let capabilities = agent(vec![runner(
            "one",
            vec![workload("a.example.io/v1", "A", small_schema("a"))],
        )]);
        let fingerprint = capabilities.schemas()[0].fingerprint().clone();
        let want = vec![fingerprint.as_str().to_owned()];
        let mut exchange = SchemaExchange::new(&capabilities);
        let sent_at = Instant::now();

        let mut attempt = HashSet::new();
        let observation = exchange.observe(&[], &want, sent_at, &mut attempt);
        assert!(observation.follow_up);
        assert!(observation.still_requested.is_empty());

        // The answer to the very request that carried it still wants it.
        let attached = exchange.attach(&mut SyncRequest::default(), sent_at, |_| 0, &attempt);
        assert_eq!(attached, vec![fingerprint.clone()]);
        let observation = exchange.observe(&attached, &want, sent_at, &mut attempt);
        assert_eq!(observation.still_requested, vec![fingerprint.clone()]);
        assert!(!observation.follow_up, "a schema just sent is not due");

        // Later attempts within five minutes neither resend it nor warn again.
        for seconds in [1, 60, 299] {
            let now = sent_at + Duration::from_secs(seconds);
            let mut attempt = HashSet::new();
            let mut request = SyncRequest::default();
            assert!(
                exchange
                    .attach(&mut request, now, |_| 0, &attempt)
                    .is_empty(),
                "resent {seconds} s after it was sent"
            );
            let observation = exchange.observe(&[], &want, now, &mut attempt);
            assert!(observation.still_requested.is_empty());
            assert!(!observation.follow_up);
        }

        // Five minutes after the send it goes out again, without a second warning.
        let now = sent_at + SCHEMA_RESEND_INTERVAL;
        let mut attempt = HashSet::new();
        let attached = exchange.attach(&mut SyncRequest::default(), now, |_| 0, &attempt);
        assert_eq!(attached, vec![fingerprint.clone()]);
        let observation = exchange.observe(&attached, &want, now, &mut attempt);
        assert!(observation.still_requested.is_empty(), "warned once");
    }

    #[test]
    fn undeclared_wanted_fingerprints_are_ignored() {
        let capabilities = agent(vec![runner(
            "one",
            vec![workload("a.example.io/v1", "A", small_schema("a"))],
        )]);
        let declared = capabilities.schemas()[0].fingerprint().clone();
        let undeclared = small_schema("declared by nobody").fingerprint().clone();
        let mut exchange = SchemaExchange::new(&capabilities);
        let now = Instant::now();
        let want = vec![
            undeclared.as_str().to_owned(),
            "NOT-A-FINGERPRINT".to_owned(),
            declared.as_str().to_owned(),
        ];

        let mut attempt = HashSet::new();
        let observation = exchange.observe(&[], &want, now, &mut attempt);
        assert_eq!(
            observation.unknown,
            vec![
                undeclared.as_str().to_owned(),
                "NOT-A-FINGERPRINT".to_owned()
            ]
        );
        assert!(observation.follow_up);

        let mut request = SyncRequest::default();
        let attached = exchange.attach(&mut request, now, |_| 0, &attempt);
        assert_eq!(attached, vec![declared.clone()]);
        assert_eq!(request.schemas.len(), 1);

        // Only undeclared fingerprints left: nothing to send, no follow-up.
        let observation = exchange.observe(&attached, &want[..2], now, &mut attempt);
        assert!(!observation.follow_up);
        assert!(observation.still_requested.is_empty());
        let mut request = SyncRequest::default();
        assert!(
            exchange
                .attach(&mut request, now, |_| 0, &HashSet::new())
                .is_empty()
        );
    }

    #[test]
    fn a_schema_goes_out_once_per_attempt() {
        let capabilities = agent(vec![runner(
            "one",
            vec![workload("a.example.io/v1", "A", small_schema("a"))],
        )]);
        let fingerprint = capabilities.schemas()[0].fingerprint().clone();
        let want = vec![fingerprint.as_str().to_owned()];
        let mut exchange = SchemaExchange::new(&capabilities);
        let sent_at = Instant::now();
        let mut attempt = HashSet::new();
        exchange.observe(&[], &want, sent_at, &mut attempt);
        let attached = exchange.attach(&mut SyncRequest::default(), sent_at, |_| 0, &attempt);
        exchange.observe(&attached, &want, sent_at, &mut attempt);

        // Slow follow-ups keep this attempt running until the schema is due again:
        // the attempt still does not send it twice.
        let late = sent_at + SCHEMA_RESEND_INTERVAL;
        assert!(
            exchange
                .attach(&mut SyncRequest::default(), late, |_| 0, &attempt)
                .is_empty()
        );
        let observation = exchange.observe(&[], &want, late, &mut attempt);
        assert!(!observation.follow_up);

        // The next attempt sends it.
        let attached = exchange.attach(&mut SyncRequest::default(), late, |_| 0, &HashSet::new());
        assert_eq!(attached, vec![fingerprint]);
    }

    #[test]
    fn a_schema_too_big_for_any_request_does_not_block_the_others() {
        let a = small_schema("a");
        let b = small_schema("b");
        // Sorted first, so that it would hold back the others.
        let big = (0..)
            .map(|i| sized_schema(&format!("too-big-{i}"), 60_000))
            .find(|big| big.fingerprint() < a.fingerprint() && big.fingerprint() < b.fingerprint())
            .expect("some tag sorts first");
        let big_fingerprint = big.fingerprint().clone();
        let capabilities = agent(vec![runner(
            "r",
            vec![
                workload("r.example.io/v1", "Big", big),
                workload("r.example.io/v1", "A", a),
                workload("r.example.io/v1", "B", b),
            ],
        )]);
        // The bare request leaves 50 000 bytes: the big schema (80 000 as base64) never fits.
        let measure = base64_measure(SYNC_REQUEST_BUDGET - 50_000);
        let mut exchange = SchemaExchange::new(&capabilities);
        let now = Instant::now();
        let mut attempt = HashSet::new();
        exchange.observe(&[], &all_wanted(&capabilities), now, &mut attempt);

        let attached = exchange.attach(&mut SyncRequest::default(), now, &measure, &attempt);

        assert_eq!(attached.len(), 2);
        assert!(!attached.contains(&big_fingerprint));
        assert_eq!(
            exchange.unsendable(&SyncRequest::default(), &measure),
            vec![big_fingerprint]
        );
    }

    #[test]
    fn a_gvk_two_runners_declare_with_one_schema_is_named_once() {
        let shared = small_schema("shared");
        let capabilities = agent(vec![
            runner(
                "first",
                vec![
                    workload("a.example.io/v1", "A", shared.clone()),
                    workload("b.example.io/v1", "B", shared.clone()),
                ],
            ),
            runner(
                "second",
                vec![workload("a.example.io/v1", "A", shared.clone())],
            ),
        ]);
        let exchange = SchemaExchange::new(&capabilities);

        assert_eq!(
            exchange.gvks(shared.fingerprint()),
            "a.example.io/v1/A, b.example.io/v1/B"
        );
        assert_eq!(
            exchange.schema_bytes(shared.fingerprint()),
            shared.canonical_json().len()
        );
    }
}
