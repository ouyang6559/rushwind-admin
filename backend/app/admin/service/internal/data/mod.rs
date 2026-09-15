//! SeaORM entities — the port of the reference's ent schemas
//! (`internal/data/ent/schema`, the single source of truth). Enum columns
//! carry the ent enum NAMES as text (ent renders native PG enums, whose
//! stored values are the same strings).

// The entity set mirrors the reference schema wholesale; entities whose
// services have not landed yet are declared but not yet referenced.
#[allow(dead_code)]
pub mod audit;
#[allow(dead_code)]
pub mod identity;
#[allow(dead_code)]
pub mod misc;
#[allow(dead_code)]
pub mod rbac;

pub use identity::*;
pub use misc::*;
pub use rbac::*;

/// `time.Now()` — wall clock, matching the reference's time.Time columns.
pub fn now() -> chrono::NaiveDateTime {
    chrono::Local::now().naive_local()
}
