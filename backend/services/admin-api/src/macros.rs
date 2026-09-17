//! Service-layer boilerplate macros shared across the per-service files.

/// The Id-only `query_by` extraction: returns the id, or fails with the
/// shared `query_by required` rejection for every other shape. The path
/// is the request's nested `QueryBy` enum.
macro_rules! query_by_id {
    ($query_by:expr, $($seg:ident)::+) => {
        match $query_by {
            Some($($seg)::+::Id(id)) => id,
            _ => return Err(crate::state::status_error("BAD_REQUEST", "query_by required")),
        }
    };
}
pub(crate) use query_by_id;
