mod bench;
mod brand;
mod clients;
mod daemon;
mod discovery;
mod identity;
mod jsonrpc;
mod manifest;
mod mock;
mod paths;
mod registry;
mod shim;
mod status;
mod wire;

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "carpool",
    version,
    about = "Share, pool and autoscale MCP servers across AI agent sessions.",
    long_about = "Carpool sits between MCP clients (Claude Code, Codex, ...) and MCP servers.\n\
                  Servers whose authors declare them safe to share (carpool.json) are pooled and\n\
                  autoscaled across sessions; every other server keeps one private process per\n\
                  session, exactly as without Carpool."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run an MCP server through Carpool. Use this as the server command in your client config.
    Run {
        /// Display name (defaults to the package name).
        #[arg(long)]
        name: Option<String>,
        /// Use this manifest instead of discovering one.
        #[arg(long)]
        manifest: Option<PathBuf>,
        /// Never share this server, whatever it declares.
        #[arg(long)]
        isolate: bool,
        /// The server command, after `--`.
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Add an MCP server to Claude Code and/or Codex, routed through Carpool.
    Add(clients::AddArgs),
    /// Remove a server added with `carpool add`.
    Remove(clients::RemoveArgs),
    /// Route the servers already configured in Claude Code / Codex through Carpool.
    Wrap(clients::WrapArgs),
    /// Undo `carpool wrap`.
    Unwrap(clients::WrapArgs),
    /// Search the official MCP Registry.
    Search {
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Show pools, instances, sessions and memory.
    Status {
        #[arg(long)]
        json: bool,
        /// Refresh every second.
        #[arg(long, short)]
        watch: bool,
    },
    /// Run the daemon in the foreground (normally started automatically).
    Daemon,
    /// Stop the daemon and every server it runs.
    Stop,
    /// Validate, scaffold or explain manifests.
    #[command(subcommand)]
    Manifest(ManifestCmd),
    /// Load-test a server through Carpool (or directly, for comparison).
    Bench(bench::BenchArgs),
    /// Print the log directory, or the log of one server.
    Logs { name: Option<String> },
    #[command(hide = true)]
    MockServer {
        #[arg(long)]
        declare: Option<String>,
        #[arg(long, default_value_t = 0)]
        startup_ms: u64,
        #[arg(long)]
        legacy_only: bool,
    },
}

#[derive(Subcommand)]
enum ManifestCmd {
    /// Validate a manifest file.
    Check { file: PathBuf },
    /// Print a starter carpool.json.
    Init {
        /// Start from a shared manifest instead of the safe default.
        #[arg(long)]
        shared: bool,
    },
    /// Show which manifest Carpool would use for a server command, and why.
    Explain {
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// List the manifests bundled in the community registry.
    List,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        // The shim stays synchronous and runtime-free to keep it ~1 MB per session.
        Cmd::Run { name, manifest, isolate, command } => shim::run(shim::RunArgs { name, manifest, isolate, command }),
        Cmd::MockServer { declare, startup_ms, legacy_only } => mock::run(mock::MockArgs { declare, startup_ms, legacy_only }),
        Cmd::Manifest(ManifestCmd::Check { file }) => manifest_check(&file),
        Cmd::Manifest(ManifestCmd::Init { shared }) => {
            println!("{}", manifest_template(shared));
            Ok(())
        }
        Cmd::Manifest(ManifestCmd::List) => {
            for (file, m) in discovery::bundled_manifests() {
                println!("{:<28} {:<8} {}", file, m.isolation, m.matches.join(", "));
            }
            Ok(())
        }
        other => async_main(other),
    }
}

#[tokio::main]
async fn async_main(cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Daemon => {
            tracing_subscriber::fmt()
                .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("CARPOOL_LOG").unwrap_or_else(|_| "info".into()))
                .with_target(false)
                .with_writer(std::io::stderr)
                .init();
            daemon::run().await
        }
        Cmd::Stop => match daemon::request(&wire::Hello::Shutdown).await {
            Ok(_) => {
                // Wait for it to release the socket so an immediate restart is clean.
                for _ in 0..100 {
                    if !paths::socket().exists() {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                println!("{} daemon stopped", brand::NAME);
                Ok(())
            }
            Err(_) => {
                println!("{} daemon is not running", brand::NAME);
                Ok(())
            }
        },
        Cmd::Status { json, watch } => status::run(json, watch).await,
        Cmd::Add(a) => clients::add(a).await,
        Cmd::Remove(a) => clients::remove(a),
        Cmd::Wrap(a) => clients::wrap(a, true),
        Cmd::Unwrap(a) => clients::wrap(a, false),
        Cmd::Search { query, limit } => registry::search(&query, limit).await,
        Cmd::Bench(a) => bench::run(a).await,
        Cmd::Logs { name } => {
            match name {
                Some(n) => println!("{}", daemon::server_log(&n).display()),
                None => println!("{}", paths::logs().display()),
            }
            Ok(())
        }
        Cmd::Manifest(ManifestCmd::Explain { command }) => manifest_explain(command).await,
        _ => unreachable!(),
    }
}

fn manifest_check(file: &PathBuf) -> Result<()> {
    let text = std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let value: serde_json::Value = serde_json::from_str(&text).context("not valid JSON")?;
    let unknown = manifest::Manifest::unknown_keys(&value);
    let m = manifest::Manifest::from_value(&value)?;
    for k in &unknown {
        eprintln!("warning: unknown key `{k}` (ignored)");
    }
    println!("✓ {} is a valid {} manifest", file.display(), brand::DISPLAY);
    println!("  isolation: {}", m.isolation);
    if m.isolation == manifest::Isolation::Shared {
        let s = &m.scaling;
        println!(
            "  partition: cwd={} env=[{}]",
            m.partition.cwd,
            m.partition.env.join(", ")
        );
        println!(
            "  scaling:   {}..{} instances, {} concurrent requests each (scale up at {}), scale down after {}s idle",
            s.min_instances, s.max_instances, s.max_concurrent_requests, s.target(), s.scale_down_after_seconds
        );
    }
    if !unknown.is_empty() {
        bail!("{} unknown key(s)", unknown.len());
    }
    Ok(())
}

fn manifest_template(shared: bool) -> String {
    let v = if shared {
        serde_json::json!({
            "$schema": brand::SCHEMA_URL,
            "specVersion": "1",
            "isolation": "shared",
            "reason": "Stateless: every tool call is independent and no per-client state is kept in memory.",
            "partition": {"env": ["MY_SERVER_API_KEY"], "cwd": false},
            "scaling": {"maxConcurrentRequests": 8, "maxInstances": 4}
        })
    } else {
        serde_json::json!({
            "$schema": brand::SCHEMA_URL,
            "specVersion": "1",
            "isolation": "session",
            "reason": "Keeps per-session state in memory."
        })
    };
    serde_json::to_string_pretty(&v).unwrap()
}

async fn manifest_explain(command: Vec<String>) -> Result<()> {
    let (cmd, args) = command.split_first().unwrap();
    let cwd = std::env::current_dir()?;
    let ident = identity::Identity::from_command(cmd, args, &cwd);
    let hello = wire::SessionHello {
        proto: wire::PROTO,
        name: String::new(),
        command: cmd.clone(),
        args: args.to_vec(),
        env: std::env::vars().collect(),
        cwd: cwd.display().to_string(),
        manifest: None,
        force_session: false,
        pid: 0,
    };
    let d = discovery::Discovery::new(false);
    let r = d.resolve(&hello, &ident);
    println!("identity:  {ident}{}", ident.version.as_ref().map(|v| format!(" (version {v})")).unwrap_or_default());
    println!("source:    {}", r.source);
    if matches!(r.source, discovery::Source::Default) && matches!(ident.kind, identity::Kind::Npm | identity::Kind::Pypi) {
        print!("remote:    ");
        match discovery::fetch_remote(&ident).await {
            Ok((Some(_), src)) => println!("published manifest found at {src} (used once cached by the daemon)"),
            Ok((None, _)) => println!("no published manifest"),
            Err(e) => println!("lookup failed: {e:#}"),
        }
    }
    println!("{}", serde_json::to_string_pretty(&r.manifest)?);
    Ok(())
}
