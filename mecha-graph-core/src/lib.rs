//! # pkg-core
//!
//! Personal knowledge graph core: ingest → enrich → resolve+link → retrieve.
//!
//! Three rules (spec §2):
//! 1. pkg-core knows nothing about any agent. No MCP, no harness concepts.
//! 2. MCP (in `pkg-mcp`) is the portability layer.
//! 3. Raw stays raw: transcripts live in their original stores; the graph
//!    holds distilled, linked knowledge with pointers back.
//!
//! The deliverable is not a database, it's a context pack (§1): every
//! interface returns a token-bounded, provenance-carrying, freshness-stamped
//! slice — see [`router::ContextPack`].

pub mod context;
pub mod corrections;
pub mod db;
pub mod decay;
pub mod embed;
pub mod enrich;
pub mod entity_audit;
pub mod episode;
pub mod error;
pub mod eval;
pub mod extract;
pub mod fact;
pub mod flags;
pub mod graph;
pub mod gtd;
pub mod ids;
pub mod integrations;
pub mod ladder;
pub mod ledger;
pub mod linkers;
pub mod llm;
pub mod migrations;
pub mod precheck;
pub mod probe;
pub mod rollup;
pub mod router;
pub mod rules;
pub mod search;
pub mod shadow;
pub mod similar;
pub mod sources;
pub mod stats;
pub mod summarize;
pub mod verify;

pub use error::{Error, Result};

/// Re-exports so consumers (pkg-cli, pkg-mcp) use the same versions.
pub use rusqlite;
pub use toml;
