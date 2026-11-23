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
//! The exchange records a send when an accepted request carries a schema, and
//! forgets it when an accepted response does not request the schema: the
//! control plane stored it, or refused it. A wanted schema is *due* when it has
//! no recorded send, or its recorded send is at least
//! [`SCHEMA_RESEND_INTERVAL`] old. So a schema that the control plane requests
//! again after it stopped, because it lost the schema, as after a restart of an
//! in-memory catalog, goes out at once. Only a schema still requested right
//! after it was sent, a store error or a fingerprint mismatch, waits for the
//! interval. A schema is *sendable* when it is due and the current attempt has
//! not sent it yet. An attempt sends each schema at most once, however long its
//! follow-ups take.

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
    /// reported once, and again only after a response stopped requesting it.
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
    /// When each schema was last carried by an accepted request, kept while
    /// every accepted response since then still requests it.
    last_sent: HashMap<SchemaFingerprint, Instant>,
    /// Fingerprints reported as still requested, kept while every accepted
    /// response since then still requests them.
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
    /// replaces the wanted set with the declared fingerprints of `want`,
    /// forgets the recorded send and the report of every schema `want` does not
    /// request, and reports what [`Observation`] describes.
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
        // The control plane stored or refused a schema it no longer requests. If
        // it requests the schema again, it lost it, so the schema is due at once.
        self.last_sent
            .retain(|fingerprint, _| self.wanted.contains(fingerprint));
        self.warned
            .retain(|fingerprint| self.wanted.contains(fingerprint));

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
    use solti_model::{WorkloadSchema, WorkloadType, WorkloadTypeMeta};

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
}

#[cfg(test)]
mod tests {}
