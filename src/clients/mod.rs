//! Client integrations: register MCP servers in Claude Code and Codex so that
//! the client launches `carpool run -- <server>` instead of the server itself.

pub mod claude;
pub mod codex;

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Args, ValueEnum};

use crate::{brand, registry};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Client {
    Claude,
    Codex,
    All,
}

impl Client {
    fn expand(list: &[Client]) -> Vec<Client> {
        if list.is_empty() || list.contains(&Client::All) {
            return vec![Client::Claude, Client::Codex];
        }
        let mut v = list.to_vec();
        v.dedup();
        v
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Scope {
    /// Claude: this project, only for you (~/.claude.json). Codex: same as user.
    Local,
    /// All projects, for you. Claude: ~/.claude.json, Codex: ~/.codex/config.toml.
    User,
    /// Checked into the repo. Claude: .mcp.json, Codex: .codex/config.toml.
    Project,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Local => "local",
            Scope::User => "user",
            Scope::Project => "project",
        }
    }
}

#[derive(Args)]
pub struct AddArgs {
    /// Name of the server in the client config.
    pub name: String,
    /// Which client(s) to configure. Repeatable. Default: all installed.
    #[arg(long = "client", short = 'c', value_enum)]
    pub clients: Vec<Client>,
    #[arg(long, short = 's', value_enum, default_value = "user")]
    pub scope: Scope,
    /// Environment variable for the server, KEY=VALUE. Repeatable.
    #[arg(long = "env", short = 'e')]
    pub env: Vec<String>,
    /// Manifest to use for this server (operator override).
    #[arg(long)]
    pub manifest: Option<PathBuf>,
    /// Never share this server.
    #[arg(long)]
    pub isolate: bool,
    /// Resolve the command from the official MCP Registry, e.g. `io.github.upstash/context7`.
    #[arg(long)]
    pub registry: Option<String>,
    /// Print what would be written without changing anything.
    #[arg(long)]
    pub dry_run: bool,
    /// The server command, after `--`.
    #[arg(last = true)]
    pub command: Vec<String>,
}

#[derive(Args)]
pub struct RemoveArgs {
    pub name: String,
    #[arg(long = "client", short = 'c', value_enum)]
    pub clients: Vec<Client>,
    #[arg(long, short = 's', value_enum, default_value = "user")]
    pub scope: Scope,
}

#[derive(Args)]
pub struct WrapArgs {
    #[arg(long = "client", short = 'c', value_enum)]
    pub clients: Vec<Client>,
    /// Only these servers (default: every stdio server).
    #[arg(long = "only")]
    pub only: Vec<String>,
    #[arg(long)]
    pub dry_run: bool,
}

/// A stdio server definition as clients store it.
#[derive(Debug, Clone, PartialEq)]
pub struct StdioServer {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

/// Absolute path to this binary, preferring the copy on PATH.
pub fn carpool_exe() -> String {
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let p = std::path::Path::new(dir).join(brand::NAME);
            if p.is_file() {
                return p.display().to_string();
            }
        }
    }
    std::env::current_exe().and_then(|p| p.canonicalize()).map(|p| p.display().to_string()).unwrap_or_else(|_| brand::NAME.into())
}

pub fn is_wrapped(s: &StdioServer) -> bool {
    let exe = std::path::Path::new(&s.command).file_name().and_then(|f| f.to_str()).unwrap_or_default();
    exe.starts_with(brand::NAME) && s.args.first().is_some_and(|a| a == "run") && s.args.iter().any(|a| a == "--")
}

/// `npx -y foo` -> `carpool run --name foo -- npx -y foo`
pub fn wrap_server(name: &str, s: &StdioServer, manifest: Option<&PathBuf>, isolate: bool) -> StdioServer {
    let mut args = vec!["run".to_string(), "--name".into(), name.to_string()];
    if let Some(m) = manifest {
        args.push("--manifest".into());
        args.push(m.canonicalize().unwrap_or(m.clone()).display().to_string());
    }
    if isolate {
        args.push("--isolate".into());
    }
    args.push("--".into());
    args.push(s.command.clone());
    args.extend(s.args.iter().cloned());
    StdioServer { command: carpool_exe(), args, env: s.env.clone() }
}

pub fn unwrap_server(s: &StdioServer) -> Option<StdioServer> {
    if !is_wrapped(s) {
        return None;
    }
    let sep = s.args.iter().position(|a| a == "--")?;
    let (command, args) = s.args[sep + 1..].split_first()?;
    Some(StdioServer { command: command.clone(), args: args.to_vec(), env: s.env.clone() })
}

pub async fn add(a: AddArgs) -> Result<()> {
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    for kv in &a.env {
        let (k, v) = kv.split_once('=').with_context(|| format!("--env {kv:?} must be KEY=VALUE"))?;
        env.insert(k.to_string(), v.to_string());
    }
    let server = match (&a.registry, a.command.split_first()) {
        (Some(id), _) => {
            let resolved = registry::resolve(id).await?;
            for var in &resolved.required_env {
                if !env.contains_key(var) {
                    eprintln!("note: {id} expects env var {var}; pass it with -e {var}=...");
                }
            }
            println!("{id} -> {} {}", resolved.command, resolved.args.join(" "));
            StdioServer { command: resolved.command, args: resolved.args, env }
        }
        (None, Some((cmd, args))) => StdioServer { command: cmd.clone(), args: args.to_vec(), env },
        (None, None) => bail!("give the server command after `--`, or use --registry <name>"),
    };
    let wrapped = wrap_server(&a.name, &server, a.manifest.as_ref(), a.isolate);
    for client in Client::expand(&a.clients) {
        match client {
            Client::Claude => claude::add(&a.name, &wrapped, a.scope, a.dry_run)?,
            Client::Codex => codex::add(&a.name, &wrapped, a.scope, a.dry_run)?,
            Client::All => unreachable!(),
        }
    }
    Ok(())
}

pub fn remove(a: RemoveArgs) -> Result<()> {
    for client in Client::expand(&a.clients) {
        match client {
            Client::Claude => claude::remove(&a.name, a.scope)?,
            Client::Codex => codex::remove(&a.name, a.scope)?,
            Client::All => unreachable!(),
        }
    }
    Ok(())
}

pub fn wrap(a: WrapArgs, wrap: bool) -> Result<()> {
    for client in Client::expand(&a.clients) {
        match client {
            Client::Claude => claude::wrap_all(&a.only, wrap, a.dry_run)?,
            Client::Codex => codex::wrap_all(&a.only, wrap, a.dry_run)?,
            Client::All => unreachable!(),
        }
    }
    if a.dry_run {
        println!("(dry run: nothing was changed)");
    } else {
        println!("Restart running Claude Code / Codex sessions to pick up the change.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_roundtrip() {
        let s = StdioServer { command: "npx".into(), args: vec!["-y".into(), "@upstash/context7-mcp".into()], env: BTreeMap::new() };
        let w = wrap_server("context7", &s, None, false);
        assert!(std::path::Path::new(&w.command).file_name().unwrap().to_str().unwrap().starts_with(brand::NAME));
        assert_eq!(w.args[..4], ["run", "--name", "context7", "--"]);
        assert!(is_wrapped(&w));
        assert_eq!(unwrap_server(&w).unwrap(), s);
        assert!(unwrap_server(&s).is_none());
    }
}
