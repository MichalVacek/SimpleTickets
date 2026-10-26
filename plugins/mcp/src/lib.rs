//! MCP server for SimpleTickets: a small JSON-RPC 2.0 subset over stdio that forwards
//! tool calls to the daemon's local HTTP API.

pub mod client;
pub mod server;
pub mod tools;
