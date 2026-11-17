//! The long-running MCP server: HTTP transport, the token, and the client registry the
//! dashboard reads. The protocol itself stays in `crate::mcp`.

pub mod http;
pub mod token;
