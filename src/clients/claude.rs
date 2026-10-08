//! Claude Code integration.
//!
//! User and local scopes live in ~/.claude.json, which Claude Code rewrites
//! while running, so changes go through the `claude mcp` CLI. Project scope is
//! the repo's .mcp.json, edited in place.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};

use super::{is_wrapped, unwrap_server, wrap_server, Scope, StdioServer};
use crate::paths;

pub fn to_json(s: &StdioServer) -> Value {
    let mut v = json!({"type": "stdio", "command": s.command, "args": s.args});
    if !s.env.is_empty() {
        v["env"] = json!(s.env);
    }
    v
}

fn from_json(v: &Value) -> Option<StdioServer> {
    let t = v.get("type").and_then(Value::as_str).unwrap_or("stdio");
    if t != "stdio" {
        return None;
    }
    Some(StdioServer {
        command: v.get("command")?.as_str()?.to_string(),
        args: v.get("args").and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default(),
        env: v
            .get("env")
            .and_then(Value::as_object)
            .map(|o| o.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect())
            .unwrap_or_default(),
    })
}

fn claude_available() -> bool {
    Command::new("claude").arg("--version").output().is_ok_and(|o| o.status.success())
}

fn claude(args: &[&str]) -> Result<()> {
    let out = Command::new("claude").args(args).output().context("running `claude` (is Claude Code installed?)")?;
    if !out.status.success() {
        bail!("`claude {}` failed: {}", args[..2].join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

fn mcp_json_path() -> PathBuf {
    std::env::current_dir().unwrap_or_default().join(".mcp.json")
}

fn edit_mcp_json(f: impl FnOnce(&mut Map<String, Value>) -> Result<()>) -> Result<()> {
    let path = mcp_json_path();
    let mut root: Value = match std::fs::read_to_string(&path) {
        Ok(t) => serde_json::from_str(&t).with_context(|| format!("parsing {}", path.display()))?,
        Err(_) => json!({}),
    };
    let servers = root.as_object_mut().context(".mcp.json is not an object")?.entry("mcpServers").or_insert_with(|| json!({}));
    f(servers.as_object_mut().context("mcpServers is not an object")?)?;
    std::fs::write(&path, serde_json::to_string_pretty(&root)? + "\n")?;
    Ok(())
}

pub fn add(name: &str, server: &StdioServer, scope: Scope, dry_run: bool) -> Result<()> {
    let config = to_json(server);
    if dry_run {
        println!("claude ({}): {name} = {}", scope.as_str(), config);
        return Ok(());
    }
    if scope == Scope::Project {
        edit_mcp_json(|servers| {
            servers.insert(name.to_string(), config.clone());
            Ok(())
        })?;
        println!("✓ Claude Code: added {name} to {}", mcp_json_path().display());
        return Ok(());
    }
    if !claude_available() {
        println!("Claude Code CLI not found. Add this to your MCP config manually:\n\"{name}\": {}", serde_json::to_string_pretty(&config)?);
        return Ok(());
    }
    // add-json refuses to overwrite; replace an existing entry of the same scope.
    let _ = claude(&["mcp", "remove", name, "--scope", scope.as_str()]);
    claude(&["mcp", "add-json", name, &config.to_string(), "--scope", scope.as_str()])?;
    println!("✓ Claude Code: added {name} ({} scope) via {}", scope.as_str(), crate::brand::NAME);
    Ok(())
}

pub fn remove(name: &str, scope: Scope) -> Result<()> {
    if scope == Scope::Project {
        edit_mcp_json(|servers| {
            servers.remove(name);
            Ok(())
        })?;
    } else {
        claude(&["mcp", "remove", name, "--scope", scope.as_str()])?;
    }
    println!("✓ Claude Code: removed {name}");
    Ok(())
}

/// Every stdio server Claude Code knows about, by scope.
fn configured() -> Vec<(Scope, String, StdioServer)> {
    let mut out = vec![];
    let cwd = std::env::current_dir().unwrap_or_default();
    if let Some(root) = std::fs::read_to_string(paths::user_home().join(".claude.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
        let mut collect = |scope: Scope, servers: Option<&Value>| {
            for (name, v) in servers.and_then(Value::as_object).into_iter().flatten() {
                if let Some(s) = from_json(v) {
                    out.push((scope, name.clone(), s));
                }
            }
        };
        collect(Scope::User, root.get("mcpServers"));
        collect(Scope::Local, root.get("projects").and_then(|p| p.get(cwd.display().to_string())).and_then(|p| p.get("mcpServers")));
    }
    if let Some(root) = std::fs::read_to_string(mcp_json_path()).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
        for (name, v) in root.get("mcpServers").and_then(Value::as_object).into_iter().flatten() {
            if let Some(s) = from_json(v) {
                out.push((Scope::Project, name.clone(), s));
            }
        }
    }
    out
}

pub fn wrap_all(only: &[String], wrap: bool, dry_run: bool) -> Result<()> {
    let mut changed = 0;
    for (scope, name, server) in configured() {
        if !only.is_empty() && !only.contains(&name) {
            continue;
        }
        let new = if wrap {
            if is_wrapped(&server) {
                continue;
            }
            wrap_server(&name, &server, None, false)
        } else {
            match unwrap_server(&server) {
                Some(s) => s,
                None => continue,
            }
        };
        println!("claude ({}) {name}: {} {}  →  {} {}", scope.as_str(), server.command, server.args.join(" "), new.command, new.args.join(" "));
        if !dry_run {
            add(&name, &new, scope, false)?;
        }
        changed += 1;
    }
    if changed == 0 {
        println!("claude: nothing to {}", if wrap { "wrap" } else { "unwrap" });
    }
    Ok(())
}

/// JSON for `claude --mcp-config`, e.g. when an orchestrator spawns sessions.
#[allow(dead_code)]
pub fn mcp_config(servers: &BTreeMap<String, StdioServer>) -> Value {
    let map: Map<String, Value> = servers.iter().map(|(n, s)| (n.clone(), to_json(&wrap_server(n, s, None, false)))).collect();
    json!({ "mcpServers": map })
}
