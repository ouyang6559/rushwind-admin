//! The differential replay library (testbed/README.md).
//!
//! [`corpus`] builds the case set: an auto-generated sweep over every route
//! in the generated table (route existence + gate envelope parity), plus the
//! curated file-backed cases. [`compare`] judges each probe pair — envelope
//! byte-parity for the gate family, envelope *shape* parity (code + reason,
//! message text normalized away — the two sides' JSON parsers produce
//! different error prose) for codec failures, routing classification, and
//! CORS header parity. [`report`] writes the JSONL record and the summary.
//!
//! The exemption map keys CASE CLASSES to justifications; exempt cases are
//! recorded with both sides' observed behavior but never fail. The seed set
//! lives in testbed/exemptions.json and must stay synchronized with the
//! divergences registered in docs/binding-spec.md and
//! docs/operator-matrix.md §5.

pub mod compare;
pub mod corpus;
pub mod report;

/// A probe outcome for one backend.
pub struct ProbeResult {
    /// Whether the backend answered anything at all (connection refused or
    /// timeout counts as unreachable; any HTTP status counts as up).
    pub reachable: bool,
    /// The HTTP status, when reachable.
    pub status: Option<u16>,
    /// The response body, when reachable.
    pub body: Option<Vec<u8>>,
    /// The CORS response headers (ACAO/ACAM/ACAC), `None` when the
    /// respective request header was absent — collected only for the
    /// Cors case kind.
    pub cors: Option<Vec<(String, Option<String>)>>,
}

impl ProbeResult {
    /// The unreachable probe.
    pub fn down() -> Self {
        Self {
            reachable: false,
            status: None,
            body: None,
            cors: None,
        }
    }
}
