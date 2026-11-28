#![forbid(unsafe_code)]

//! ags — Cross Agent Session Resumer.
//!
//! Library entry point exposing the public API for session conversion.
//! The binary (`main.rs`) is a thin CLI wrapper around this library.

pub mod agent_runtime;
pub mod berth;
pub mod budget;
pub mod bundle;
pub mod cage_shim;
pub mod checkpoint_runtime;
pub mod compare;
pub mod conformance;
pub mod discovery;
pub mod environment;
pub mod environment_launch;
pub mod environment_runtime;
pub mod error;
pub mod host_mounts;
pub mod ir;
pub mod launch;
pub mod listing_cache;
pub mod macos_vm;
pub mod model;
pub mod pipeline;
pub mod providers;
pub mod rebind_continue;
pub mod redact;
pub mod replay;
pub mod responses;
pub mod sandbox;
pub mod self_update;
pub mod session_hub;
pub mod store;
pub mod vault_sync;
