//! SeaORM entities — the full schema
//! catalog. Enum columns
//! carry the ent enum NAMES as text (ent renders native PG enums, whose
//! stored values are the same strings).

// The entity set covers the full schema; entities whose
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

pub mod repos;
pub mod scope;

/// `time.Now()` — wall clock for timestamp columns.
pub fn now() -> chrono::NaiveDateTime {
    chrono::Local::now().naive_local()
}
