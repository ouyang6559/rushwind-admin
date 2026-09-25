//! The transport servers. The gateway surface is a hand-written axum
//! route pack (line-compatible form endpoints, not proto codegen); the
//! task-queue worker + cron producer (`server/worker.rs`) run the T+1
//! thaw schedule; the back-office surfaces (protobuf + JWT + RBAC)
//! mount here from Phase 7.

pub mod gateway;
pub mod panel;
pub mod worker;
