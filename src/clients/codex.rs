//! OpenAI Codex integration: `[mcp_servers.<name>]` tables in
//! `$CODEX_HOME/config.toml` (user scope) or `.codex/config.toml` (project).
//! Edits preserve comments and formatting.
//!
//! Codex passes stdio servers only a whitelisted environment plus the
//! server's `env` / `env_vars`; Carpool sees exactly that, so a wrapped server
//! gets the same variables it would have had without Carpool.

use std::path::PathBuf;

use anyhow::{Context, Result};
use toml_edit::{value, Array, DocumentMut, InlineTable, Item, Table};

use super::{is_wrapped, unwrap_server, wrap_server, Scope, StdioServer};
use crate::paths;

pub fn config_path(scope: Scope) -> PathBuf {
    match scope {
        Scope::Project => std::env::current_dir().unwrap_or_default().join(".codex").join("config.toml"),
        _ => std::env::var_os("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| paths::user_home().join(".codex")).join("config.toml"),
    }
}

fn load(path: &PathBuf) -> Result<DocumentMut> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.parse::<DocumentMut>().with_context(|| format!("parsing {}", path.display()))
}

fn save(path: &PathBuf, doc: &DocumentMut) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, doc.to_string())?;
    Ok(())
}

fn servers(doc: &mut DocumentMut) -> &mut Table {
    let item = doc.entry("mcp_servers").or_insert_with(|| {
        let mut t = Table::new();
        t.set_implicit(true);
        Item::Table(t)
    });
    item.as_table_mut().expect("mcp_servers must be a table")
}

fn read(t: &Table) -> Option<StdioServer> {
    Some(StdioServer {
        command: t.get("command")?.as_str()?.to_string(),
        args: t.get("args").and_then(Item::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default(),
        env: t
            .get("env")
            .and_then(|e| e.as_table_like())
            .map(|e| e.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.to_string(), s.to_string()))).collect())
            .unwrap_or_default(),
    })
}

/// Write command/args/env into `t`, leaving every other key (env_vars,
/// tool_timeout_sec, enabled_tools, ...) untouched.
fn write(t: &mut Table, s: &StdioServer) {
    t["command"] = value(&s.command);
    let mut args = Array::new();
    for a in &s.args {
        args.push(a.as_str());
    }
    t["args"] = value(args);
    if s.env.is_empty() {
        t.remove("env");
    } else {
        let mut env = InlineTable::new();
        for (k, v) in &s.env {
            env.insert(k, v.as_str().into());
        }
        t["env"] = value(env);
    }
}

pub fn add(name: &str, server: &StdioServer, scope: Scope, dry_run: bool) -> Result<()> {
    let path = config_path(scope);
    let mut doc = load(&path)?;
    let table = servers(&mut doc).entry(name).or_insert_with(|| Item::Table(Table::new()));
    let t = table.as_table_mut().context("server entry is not a table")?;
    write(t, server);
    // First start may download the package; Codex's default is 10s.
    if t.get("startup_timeout_sec").is_none() {
        t["startup_timeout_sec"] = value(60);
    }
    if dry_run {
        println!("codex ({}):\n[mcp_servers.{name}]\n{}", path.display(), t);
        return Ok(());
    }
    save(&path, &doc)?;
    println!("✓ Codex: added {name} to {}", path.display());
    Ok(())
}

pub fn remove(name: &str, scope: Scope) -> Result<()> {
    let path = config_path(scope);
    let mut doc = load(&path)?;
    servers(&mut doc).remove(name);
    save(&path, &doc)?;
    println!("✓ Codex: removed {name}");
    Ok(())
}

pub fn wrap_all(only: &[String], wrap: bool, dry_run: bool) -> Result<()> {
    let path = config_path(Scope::User);
    if !path.exists() {
        println!("codex: no config at {}", path.display());
        return Ok(());
    }
    let mut doc = load(&path)?;
    let mut changed = 0;
    for (name, item) in servers(&mut doc).iter_mut() {
        if !only.is_empty() && !only.iter().any(|o| o == name.get()) {
            continue;
        }
        let Some(t) = item.as_table_mut() else { continue };
        let Some(current) = read(t) else { continue }; // HTTP servers have no command
        let new = if wrap {
            if is_wrapped(&current) {
                continue;
            }
            wrap_server(name.get(), &current, None, false)
        } else {
            match unwrap_server(&current) {
                Some(s) => s,
                None => continue,
            }
        };
        println!("codex {}: {} {}  →  {} {}", name.get(), current.command, current.args.join(" "), new.command, new.args.join(" "));
        write(t, &new);
        changed += 1;
    }
    if changed == 0 {
        println!("codex: nothing to {}", if wrap { "wrap" } else { "unwrap" });
    } else if !dry_run {
        save(&path, &doc)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_preserves_other_keys_and_comments() {
        let src = "# my config\nmodel = \"gpt-5\"\n\n[mcp_servers.time]\ncommand = \"uvx\"\nargs = [\"mcp-server-time\"]\ntool_timeout_sec = 30 # keep\nenv_vars = [\"TZ\"]\n\n[mcp_servers.remote]\nurl = \"https://example.com/mcp\"\n";
        let mut doc: DocumentMut = src.parse().unwrap();
        let t = servers(&mut doc).get_mut("time").unwrap().as_table_mut().unwrap();
        let cur = read(t).unwrap();
        write(t, &wrap_server("time", &cur, None, false));
        let out = doc.to_string();
        assert!(out.starts_with("# my config\nmodel = \"gpt-5\""));
        assert!(out.contains("tool_timeout_sec = 30 # keep"));
        assert!(out.contains("env_vars = [\"TZ\"]"));
        assert!(out.contains("\"run\", \"--name\", \"time\", \"--\", \"uvx\", \"mcp-server-time\""));
        assert!(out.contains("url = \"https://example.com/mcp\""));
        let mut doc2: DocumentMut = out.parse().unwrap();
        let t2 = servers(&mut doc2).get_mut("time").unwrap().as_table_mut().unwrap();
        assert_eq!(unwrap_server(&read(t2).unwrap()).unwrap(), cur);
    }
}
