//! Codexify MCP bridge, ported to Rust.
//!
//! A local Streamable-HTTP MCP server exposing Codex-style agent tools over a
//! chosen work directory. See the module docs for the piece-by-piece port of the
//! original TypeScript.

pub mod apply_patch;
pub mod artifact_egress;
pub mod artifact_ingress;
mod audit;
pub mod auth;
pub mod bridge;
pub mod codex_config;
pub mod codex_mcp;
mod codex_plugin_skills;
pub mod config;
pub mod conversation_auth;
pub mod diff;
pub mod diff_ui;
pub mod environment;
pub mod exec_policy;
pub mod exec_sessions;
pub mod ignore_rules;
pub mod instructions;
pub mod legacy_migration;
pub mod logging;
mod mcp_catalog;
pub mod memory;
pub mod openai_tunnel;
pub mod output_budget;
pub mod process_env;
pub mod project_bindings;
pub mod project_catalog;
pub mod project_clone;
pub mod project_doc;
pub mod quickstart;
mod redaction;
pub mod registry;
pub mod safe_path;
pub mod self_update;
pub mod server;
pub mod service;
pub mod skills;
pub mod tls;
pub mod tool;
mod tool_logging;
pub mod tools;
pub mod types;
pub mod util;
pub mod worktrees;
