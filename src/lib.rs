//! Codexify MCP bridge, ported to Rust.
//!
//! A local Streamable-HTTP MCP server exposing Codex-style agent tools over a
//! chosen work directory. See the module docs for the piece-by-piece port of the
//! original TypeScript.

pub mod apply_patch;
mod audit;
pub mod auth;
pub mod bridge;
pub mod codex_config;
pub mod codex_mcp;
pub mod config;
pub mod environment;
pub mod exec_policy;
pub mod exec_sessions;
pub mod ignore_rules;
pub mod instructions;
pub mod logging;
pub mod memory;
pub mod openai_tunnel;
pub mod output_budget;
pub mod process_env;
pub mod project_bindings;
pub mod project_catalog;
pub mod project_doc;
pub mod quickstart;
pub mod registry;
pub mod review;
pub mod review_ui;
pub mod safe_path;
pub mod server;
pub mod skills;
pub mod tool;
pub mod tools;
pub mod types;
pub mod util;
