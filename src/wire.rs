//! Protocol between the per-session shim and the daemon, over a Unix socket.
//!
//! shim -> daemon: one JSON `Hello` line, then the client's MCP stream verbatim
//!                 (newline-delimited JSON-RPC).
//! daemon -> shim: lines tagged by their first byte:
//!                 `M<json>`  MCP message for the client (stdout)
//!                 `E<text>`  server stderr / diagnostics (stderr)
//!                 `X<code>`  the server is gone; exit with this code

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTO: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum Hello {
    Session(SessionHello),
    Status,
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHello {
    pub proto: u32,
    /// Name the client gave the server in its config (informational).
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: String,
    /// Inline manifest from `carpool run --manifest`.
    #[serde(default)]
    pub manifest: Option<Value>,
    /// `carpool run --isolate`: never share, whatever the server declares.
    #[serde(default)]
    pub force_session: bool,
    /// PID of the shim, for status output.
    #[serde(default)]
    pub pid: u32,
}

pub fn msg_frame(json: &str) -> String {
    let mut s = String::with_capacity(json.len() + 2);
    s.push('M');
    s.push_str(json);
    s.push('\n');
    s
}

pub fn stderr_frame(text: &str) -> String {
    format!("E{}\n", text.replace('\n', " "))
}

pub fn exit_frame(code: i32) -> String {
    format!("X{code}\n")
}
