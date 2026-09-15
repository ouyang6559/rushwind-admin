//! The admin service library face: configuration, the shared runtime
//! state, the SeaORM data layer, the per-service implementations, and
//! the REST/SSE/task transports. The binary (`main.rs`) is a thin
//! assembly entry over this surface.

pub mod assets;
pub mod audit;
pub mod captcha;
pub mod config;
pub mod crypto;
pub mod data;
pub mod migration;
pub mod paging;
pub mod policy;
pub mod ratelimit;
pub mod seed;
pub mod server;
pub mod services;
pub mod state;
pub mod token;
