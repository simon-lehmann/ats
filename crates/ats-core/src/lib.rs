//! Shared types for ATS: RPC protocol, session state, config.
//!
//! This crate is the contract between the daemon, the TUI, and the CLI.
//! See docs/PLAN.md §3 for the protocol design.

pub mod config;
pub mod rpc;
pub mod state;
