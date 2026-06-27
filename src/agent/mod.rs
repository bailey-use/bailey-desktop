//! aivo's native agent, driven by `aivo chat`. The tool-using loop runs
//! in-process in `engine`: it composes OpenAI chat requests, calls the model
//! through the loopback serve (the sole network egress), executes `tools`
//! locally, and renders through an `AgentUi` (the chat TUI). `protocol` holds
//! the shared data types; `serve_client` is the streaming provider call.

pub mod apply_patch;
pub mod checkpoint;
pub mod engine;
pub mod mcp;
pub mod notes;
pub mod plan;
pub mod protocol;
pub mod sandbox;
pub mod serve_client;
pub mod skills;
pub mod subagents;
pub mod tools;
