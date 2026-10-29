//! safe-grokbot-bridge: a secure HTTP bridge between a remote AI agent and the coding agents
//! running on this machine.
//!
//! The binary (`src/main.rs`) is a thin CLI over this library. See `docs/design.md` for the
//! architecture and `CLAUDE.md` for the module layout.

pub mod agents;
pub mod api;
pub mod auth;
pub mod config;
pub mod error;
pub mod exec;
pub mod files;
pub mod git;
pub mod hook;
pub mod paths;
pub mod server;
pub mod tls;
