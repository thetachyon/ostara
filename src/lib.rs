//! ostara: high-performance Rust reimplementation of the OSIRIS
//! (osirisai.live) open-source intelligence platform's API core.
//!
//! Scope (v0.1): feed aggregation, normalization, caching, and HTTP
//! delivery. The WebGL globe UI is explicitly out of scope.
//!
//! The crate is split into a library (so criterion benchmarks and tests can
//! exercise the parse/normalize hot paths without networking) and a thin
//! binary (`src/main.rs`) that wires up the axum server.

pub mod cache;
pub mod config;
pub mod country_inspect;
pub mod country_osint;
pub mod dossier;
pub mod error;
pub mod feeds;
pub mod geo;
pub mod graph;
pub mod intel;
pub mod recon;
pub mod routes;
pub mod time;
