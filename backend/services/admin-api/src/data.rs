//! The SeaORM entity catalog for the full schema. Enum columns carry
//! the enum names as text.

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

/// Wall clock for timestamp columns.
pub fn now() -> chrono::NaiveDateTime {
    chrono::Local::now().naive_local()
}
