#![forbid(unsafe_code)]

//! ags — Cross Agent Session Resumer.
//!
//! Library entry point exposing the public API for session conversion.
//! The binary (`main.rs`) is a thin CLI wrapper around this library.

pub mod budget;
pub mod checkpoint_runtime;
pub mod compare;
pub mod conformance;
pub mod discovery;
pub mod error;
pub mod ir;
pub mod launch;
pub mod listing_cache;
pub mod model;
pub mod pipeline;
pub mod providers;
pub mod replay;
pub mod responses;
pub mod store;
