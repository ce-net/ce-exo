//! # ce-exo-core — the shared substrate for ce-exo
//!
//! ce-exo runs LLMs across a fleet of CE nodes. It is an **app on CE primitives**: every byte of
//! coordination rides the CE mesh ([`ce-rs`] `request`/`reply`/`stream`), authorization is a
//! [`ce-cap`] capability chain, and host selection reuses [`ce-lb`]. This crate is the pure core
//! shared by the worker, router, SDK and CLI — no networking, no process spawning, fully unit-tested.
//!
//! ## The two distribution modes (exo parity)
//!
//! - **Replica** — each node holds the *whole* model; many requests are load-balanced across the
//!   replicas for throughput. Used when the model fits in a single node's memory budget.
//! - **Pipeline** — one model is too big for any single node, so its transformer layer stack is
//!   split into contiguous ranges, one range per node (a memory-weighted ring). Only the boundary
//!   activation tensor (~KB/token) crosses the wire between stages — never weight shards, never
//!   tensor-parallel all-reduces over Ethernet.
//!
//! The headline ce-exo concept is that **you never pick the mode**: [`plan::plan`] inspects the
//! model size against live fleet capacity and chooses replica-vs-pipeline automatically, building
//! the minimal placement that fits.

pub mod caps;
pub mod plan;
pub mod probe;
pub mod proto;
pub mod registry;

pub use plan::{Mode, NodeCap, Placement, PlanOpts, Stage};
pub use probe::{HardwareProbe, MemClass};
pub use registry::{ModelEntry, Registry};

/// Crate-wide protocol version. Bump on any breaking wire change; workers advertise it so the router
/// can refuse to dispatch to an incompatible peer.
pub const PROTOCOL_VERSION: u32 = 1;
