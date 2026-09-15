//! The case corpus: an auto-generated sweep plus the curated file cases.
//!
//! The sweep walks the generated route table (the same 203 registrations the
//! Go backend makes) and emits, per route, a matched-method probe against
//! the concrete path (path variables substituted with `1`) classified as:
//!
//! * gated routes — [`Kind::EnvelopeExact`]: both sides must answer the
//!   identical 401 `UNAUTHORIZED` / `missing bearer token` envelope bytes;
//! * public routes — [`Kind::Pending`]: the Rust side is a null stub until
//!   its module lands; both responses are recorded, nothing asserted.
//!
//! GET routes additionally emit a HEAD probe (class `head-on-get`) — the
//! recorded micro-divergence where axum's `MethodFilter::GET` serves HEAD
//! while gorilla mux rejects it; the class is in the seed exemption set and
//! the probe merely quantifies it.
//!
//! The curated file (testbed/corpus/curated.json) carries the hand-built
//! cases: gate rejections with malformed bearer credentials, codec failures
//! on public body routes, and CORS preflights.

use std::collections::HashMap;

use serde::Deserialize;

use crate::compare::Kind;

/// One replay case.
#[derive(Debug, Clone)]
pub struct Case {
    /// Stable identifier (`sweep-<idx>`, `sweep-head-<idx>`, or the curated id).
    pub id: String,
    /// The exemption class (`sweep-gated`, `sweep-public`, `head-on-get`, or
    /// the curated class).
    pub class: String,
    /// The comparison contract.
    pub kind: Kind,
    /// HTTP method, uppercase.
    pub method: String,
    /// The concrete path (path variables substituted).
    pub path: String,
    /// Extra request headers.
    pub headers: Vec<(String, String)>,
    /// The request body, if any.
    pub body: Option<String>,
}

/// The file face of a curated case; mirrors [`Case`] minus the derived id.
#[derive(Debug, Deserialize)]
struct CuratedCase {
    id: String,
    class: String,
    kind: KindWire,
    method: String,
    path: String,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(default)]
    body: Option<String>,
}

/// The wire spelling of the case kind.
#[derive(Debug, Deserialize)]
enum KindWire {
    #[serde(rename = "Routing")]
    Routing,
    #[serde(rename = "EnvelopeExact")]
    EnvelopeExact,
    #[serde(rename = "EnvelopeShape")]
    EnvelopeShape,
    #[serde(rename = "Cors")]
    Cors,
    #[serde(rename = "Pending")]
    Pending,
}

impl From<&KindWire> for Kind {
    fn from(w: &KindWire) -> Self {
        match w {
            KindWire::Routing => Kind::Routing,
            KindWire::EnvelopeExact => Kind::EnvelopeExact,
            KindWire::EnvelopeShape => Kind::EnvelopeShape,
            KindWire::Cors => Kind::Cors,
            KindWire::Pending => Kind::Pending,
        }
    }
}

/// Substitutes every `{var}` segment in a route template with `1`.
fn concretize(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut rest = path;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        match rest[open..].find('}') {
            Some(close) => {
                out.push('1');
                rest = &rest[open + close + 1..];
            }
            None => {
                rest = &rest[open..];
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Builds the sweep from the generated route table.
pub fn sweep() -> Vec<Case> {
    let mut cases = Vec::new();
    for (idx, spec) in gen_rust::gen::routes::ROUTES.iter().enumerate() {
        let concrete = concretize(spec.path);
        let gated = !gen_rust::AUTH_FREE
            .iter()
            .any(|(s, m)| *s == spec.service_fq && *m == spec.method_name);
        let (class, kind) = if spec.shadowed {
            // The shadow set: the go stack's first-match mux routes
            // these paths to the earlier pattern route, whose path-variable
            // bind is malformed for the literal segment — the pre/post-auth
            // ordering divergence the exemption set registers.
            ("router-shadow", Kind::EnvelopeShape)
        } else if gated {
            ("sweep-gated", Kind::EnvelopeExact)
        } else {
            ("sweep-public", Kind::Pending)
        };
        cases.push(Case {
            id: format!("sweep-{idx}"),
            class: class.into(),
            kind,
            method: spec.method.to_ascii_uppercase(),
            path: concrete.clone(),
            headers: Vec::new(),
            body: None,
        });
        if spec.method.eq_ignore_ascii_case("GET") {
            cases.push(Case {
                id: format!("sweep-head-{idx}"),
                class: "head-on-get".into(),
                kind: Kind::Routing,
                method: "HEAD".into(),
                path: concrete,
                headers: Vec::new(),
                body: None,
            });
        }
    }
    cases
}

/// Loads the curated cases from `<dir>/curated.json`.
pub fn load_curated(dir: &str) -> Vec<Case> {
    let path = std::path::Path::new(dir).join("curated.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        eprintln!("curated corpus missing: {} (skipping)", path.display());
        return Vec::new();
    };
    match serde_json::from_str::<Vec<CuratedCase>>(&text) {
        Ok(list) => list
            .into_iter()
            .map(|c| Case {
                id: c.id,
                class: c.class,
                kind: Kind::from(&c.kind),
                method: c.method.to_ascii_uppercase(),
                path: c.path,
                headers: c.headers.into_iter().collect(),
                body: c.body,
            })
            .collect(),
        Err(e) => {
            eprintln!("curated corpus parse error: {e} (skipping)");
            Vec::new()
        }
    }
}

/// Loads the exemption map (class → justification) from the given file.
pub fn load_exemptions(path: &str) -> HashMap<String, String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        eprintln!("exemption file missing: {path} (none applied)");
        return HashMap::new();
    };
    #[derive(Deserialize)]
    struct File {
        exemptions: Vec<Entry>,
    }
    #[derive(Deserialize)]
    struct Entry {
        class: String,
        note: String,
    }
    match serde_json::from_str::<File>(&text) {
        Ok(f) => f
            .exemptions
            .into_iter()
            .map(|e| (e.class, e.note))
            .collect(),
        Err(e) => {
            eprintln!("exemption file parse error: {e} (none applied)");
            HashMap::new()
        }
    }
}
