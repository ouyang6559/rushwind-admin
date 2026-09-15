//! The comparators.
//!
//! Envelope judgments are strict on the pairs the frontend switches on:
//! HTTP status and the envelope's `code`/`reason`; the gate family
//! ([`Kind::EnvelopeExact`]) additionally pins the full body bytes — the
//! Go middleware's message literals are part of the wire contract — while
//! codec failures ([`Kind::EnvelopeShape`]) normalize the message text away
//! (the two sides' JSON parsers emit different prose for the same
//! malformed input). Routing judges only the matched/unmatched split
//! (404/405 = unmatched). CORS judges the ACAO/ACAM/ACAC header pair.
//!
//! [`semantic_diffs`] is the data-plane comparator for the pending corpus
//! (and every future module once real implementations land): a JSON-tree
//! walk with map-unordered/array-ordered comparison and identity/timestamp
//! normalization, per docs/development-plan.md's differential contract.

use std::collections::HashMap;

use crate::ProbeResult;

/// The comparison contract of a case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Both sides must agree on matched vs unmatched (404/405 = unmatched).
    Routing,
    /// Status and full body bytes must be identical.
    EnvelopeExact,
    /// Status and the envelope's `code`/`reason` must be identical; the
    /// `message` text is normalized away.
    EnvelopeShape,
    /// The ACAO/ACAM/ACAC response headers must agree (presence and value).
    Cors,
    /// Both sides recorded, nothing asserted (Rust stub until modules land).
    Pending,
}

/// The verdict of one judged case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Both sides behaved identically under the case's contract.
    Ok,
    /// Divergence beyond the registered exemptions.
    Fail,
    /// The case's class is registered in the exemption set; recorded, not
    /// failed.
    Exempt,
    /// Both sides recorded, nothing asserted.
    Pending,
    /// One or both backends were unreachable.
    Unreachable,
}

/// Judges one probe pair.
pub fn judge(
    kind: Kind,
    class: &str,
    go: &ProbeResult,
    rust: &ProbeResult,
    exemptions: &HashMap<String, String>,
) -> (Verdict, Option<String>) {
    if !go.reachable || !rust.reachable {
        return (Verdict::Unreachable, None);
    }
    if let Some(note) = exemptions.get(class) {
        return (Verdict::Exempt, Some(note.clone()));
    }
    match kind {
        Kind::Routing => judge_routing(go, rust),
        Kind::EnvelopeExact => judge_envelope(go, rust, true),
        Kind::EnvelopeShape => judge_envelope(go, rust, false),
        Kind::Cors => judge_cors(go, rust),
        Kind::Pending => (Verdict::Pending, None),
    }
}

/// Whether a response counts as route-unmatched on its stack (gorilla mux
/// answers 404 for unknown paths and 405 for method mismatches; axum
/// answers 404/405 the same way for unmatched path/method pairs).
fn unmatched(status: Option<u16>) -> Option<bool> {
    match status {
        Some(404) | Some(405) => Some(true),
        Some(_) => Some(false),
        None => None,
    }
}

fn judge_routing(go: &ProbeResult, rust: &ProbeResult) -> (Verdict, Option<String>) {
    let (Some(go_unmatched), Some(rust_unmatched)) = (unmatched(go.status), unmatched(rust.status))
    else {
        return (
            Verdict::Fail,
            Some(format!(
                "routing probe got no status: go={:?} rust={:?}",
                go.status, rust.status
            )),
        );
    };
    if go_unmatched == rust_unmatched {
        (Verdict::Ok, None)
    } else {
        (
            Verdict::Fail,
            Some(format!(
                "route classification diverged: go={} (status {:?}) vs rust={} (status {:?})",
                if go_unmatched { "unmatched" } else { "matched" },
                go.status,
                if rust_unmatched {
                    "unmatched"
                } else {
                    "matched"
                },
                rust.status
            )),
        )
    }
}

/// The envelope fields both sides must agree on: the envelope must parse as
/// a JSON object carrying all four Kratos envelope keys, and the `code` and
/// `reason` values must be identical (and equal to the HTTP status for the
/// code — the Go side passes the same number to both). With `exact`, the
/// full body bytes must also be identical.
fn judge_envelope(go: &ProbeResult, rust: &ProbeResult, exact: bool) -> (Verdict, Option<String>) {
    if go.status != rust.status {
        return (
            Verdict::Fail,
            Some(format!(
                "status diverged: go={:?} rust={:?}",
                go.status, rust.status
            )),
        );
    }
    let body_of = |p: &ProbeResult| -> Option<serde_json::Value> {
        p.body
            .as_ref()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(b).ok())
    };
    let fields = |v: &serde_json::Value| -> Option<(i64, String)> {
        let obj = v.as_object()?;
        if !["code", "reason", "message", "metadata"]
            .iter()
            .all(|k| obj.contains_key(*k))
        {
            return None;
        }
        let code = obj.get("code")?.as_i64()?;
        let reason = obj.get("reason")?.as_str()?.to_string();
        Some((code, reason))
    };
    let (Some(go_v), Some(rust_v)) = (body_of(go), body_of(rust)) else {
        return (
            Verdict::Fail,
            Some("one side's envelope did not parse as JSON".into()),
        );
    };
    let (Some(go_f), Some(rust_f)) = (fields(&go_v), fields(&rust_v)) else {
        return (
            Verdict::Fail,
            Some("one side's envelope lacks the four-field shape".into()),
        );
    };
    if go_f != rust_f {
        return (
            Verdict::Fail,
            Some(format!(
                "envelope code/reason diverged: go={go_f:?} rust={rust_f:?}"
            )),
        );
    }
    if exact {
        let go_bytes = go.body.clone().unwrap_or_default();
        let rust_bytes = rust.body.clone().unwrap_or_default();
        if go_bytes != rust_bytes {
            return (
                Verdict::Fail,
                Some(format!(
                    "envelope bytes diverged beyond code/reason: go={:?} rust={:?}",
                    String::from_utf8_lossy(&go_bytes),
                    String::from_utf8_lossy(&rust_bytes)
                )),
            );
        }
    }
    (Verdict::Ok, None)
}

/// Compares the CORS response header pair on both sides.
fn judge_cors(go: &ProbeResult, rust: &ProbeResult) -> (Verdict, Option<String>) {
    let (Some(go_h), Some(rust_h)) = (&go.cors, &rust.cors) else {
        return (
            Verdict::Fail,
            Some("cors probe lost its header capture".into()),
        );
    };
    let by_name = |headers: &[(String, Option<String>)]| {
        headers
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
            .collect::<HashMap<_, _>>()
    };
    let (go_map, rust_map) = (by_name(go_h), by_name(rust_h));
    if go_map != rust_map {
        return (
            Verdict::Fail,
            Some(format!(
                "cors response headers diverged: go={go_map:?} rust={rust_map:?}"
            )),
        );
    }
    (Verdict::Ok, None)
}

// ---------------------------------------------------------------------------
// The semantic data-plane comparator (pending corpus; per-module corpus
// once real implementations land).
// ---------------------------------------------------------------------------

/// Whether a JSON key names an identity field: values are server-generated
/// and normalized away. Over-matching only weakens detection on that key;
/// the one real collision in the corpus (`valid`) is excluded.
fn is_identity_key(key: &str) -> bool {
    let lowered = key.to_ascii_lowercase();
    if lowered == "id" || lowered.ends_with("_id") || lowered.ends_with("_by") {
        return true;
    }
    lowered.ends_with("id") && lowered.len() > 2 && lowered != "valid"
}

/// Whether a JSON key names a timestamp field.
fn is_timestamp_key(key: &str) -> bool {
    let lowered = key.to_ascii_lowercase();
    [
        "created_at",
        "updated_at",
        "deleted_at",
        "create_time",
        "update_time",
    ]
    .contains(&lowered.as_str())
        || lowered.ends_with("_at")
}

/// Whether a scalar string looks like a generated identifier or timestamp.
fn normalized_scalar(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    if let (Some(a), Some(b)) = (a.as_str(), b.as_str()) {
        if looks_like_uuid(a) && looks_like_uuid(b) {
            return true;
        }
        if looks_like_timestamp(a) && looks_like_timestamp(b) {
            return true;
        }
    }
    a == b
}

fn looks_like_uuid(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, b)| {
            if i == 8 || i == 13 || i == 18 || i == 23 {
                *b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

fn looks_like_timestamp(s: &str) -> bool {
    s.len() >= 19 && s.as_bytes()[..4].iter().all(|b| b.is_ascii_digit()) && s.as_bytes()[4] == b'-'
}

/// The semantic walk: maps unordered, arrays ordered, identity/timestamp
/// values normalized; every other divergence appended to `diffs` with its
/// JSON path.
pub fn semantic_diffs(
    a: &serde_json::Value,
    b: &serde_json::Value,
    path: &str,
    diffs: &mut Vec<String>,
) {
    match (a, b) {
        (serde_json::Value::Object(ao), serde_json::Value::Object(bo)) => {
            for key in ao.keys().collect::<Vec<_>>() {
                if !bo.contains_key(key) {
                    diffs.push(format!("{path}.{key}: missing on rust side"));
                }
            }
            for key in bo.keys().collect::<Vec<_>>() {
                if !ao.contains_key(key) {
                    diffs.push(format!("{path}.{key}: missing on go side"));
                }
            }
            for key in ao.keys() {
                let Some(bv) = bo.get(key) else { continue };
                if is_identity_key(key) || is_timestamp_key(key) {
                    continue;
                }
                semantic_diffs(&ao[key], bv, &format!("{path}.{key}"), diffs);
            }
        }
        (serde_json::Value::Array(aa), serde_json::Value::Array(ba)) => {
            if aa.len() != ba.len() {
                diffs.push(format!("{path}: array length {} vs {}", aa.len(), ba.len()));
            }
            for (i, (av, bv)) in aa.iter().zip(ba.iter()).enumerate() {
                semantic_diffs(av, bv, &format!("{path}[{i}]"), diffs);
            }
        }
        (a, b) => {
            if !normalized_scalar(a, b) {
                diffs.push(format!("{path}: value diverged"));
            }
        }
    }
}
