//! `carpool bench`: simulate many agent sessions calling one MCP server, through
//! Carpool or directly (one process per session, as clients do today), and
//! report latency, process count and memory.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::Args;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;

use crate::wire::{Hello, SessionHello, PROTO};
use crate::{paths, status};

#[derive(Args)]
pub struct BenchArgs {
    /// Concurrent client sessions (agents).
    #[arg(long, short = 'n', default_value_t = 50)]
    pub sessions: usize,
    /// Sequential tool calls per session.
    #[arg(long, short = 'k', default_value_t = 20)]
    pub calls: usize,
    /// Tool to call (default: first tool from tools/list).
    #[arg(long)]
    pub tool: Option<String>,
    /// JSON arguments for the tool.
    #[arg(long, default_value = "{}")]
    pub arguments: String,
    /// Delay between session starts, in ms.
    #[arg(long, default_value_t = 0)]
    pub ramp_ms: u64,
    /// Keep sessions open this many seconds after the calls (to inspect `carpool status`).
    #[arg(long, default_value_t = 0)]
    pub hold: u64,
    /// Bypass Carpool: spawn one server process per session, like clients do today.
    #[arg(long)]
    pub direct: bool,
    /// Use the stateless (2026-07-28) protocol instead of initialize.
    #[arg(long)]
    pub stateless: bool,
    #[arg(long)]
    pub manifest: Option<PathBuf>,
    #[arg(long)]
    pub isolate: bool,
    /// Name shown in `carpool status`.
    #[arg(long, default_value = "bench")]
    pub name: String,
    #[arg(last = true, required = true)]
    pub command: Vec<String>,
}

enum Conn {
    Carpool { lines: Lines<BufReader<tokio::net::unix::OwnedReadHalf>>, w: tokio::net::unix::OwnedWriteHalf },
    Direct { lines: Lines<BufReader<tokio::process::ChildStdout>>, w: tokio::process::ChildStdin, _child: Box<tokio::process::Child> },
}

impl Conn {
    async fn send(&mut self, v: &Value) -> Result<()> {
        let line = format!("{v}\n");
        match self {
            Conn::Carpool { w, .. } => w.write_all(line.as_bytes()).await?,
            Conn::Direct { w, .. } => w.write_all(line.as_bytes()).await?,
        }
        Ok(())
    }

    async fn recv(&mut self) -> Result<Value> {
        loop {
            let line = match self {
                Conn::Carpool { lines, .. } => lines.next_line().await?,
                Conn::Direct { lines, .. } => lines.next_line().await?,
            }
            .context("server closed the connection")?;
            let json = match self {
                Conn::Carpool { .. } => match line.strip_prefix('M') {
                    Some(j) => j.to_string(),
                    None if line.starts_with('X') => bail!("server exited"),
                    None => continue,
                },
                Conn::Direct { .. } => line,
            };
            if let Ok(v) = serde_json::from_str::<Value>(&json) {
                if v.get("id").is_some() && v.get("method").is_none() {
                    return Ok(v);
                }
            }
        }
    }

    async fn call(&mut self, req: Value) -> Result<Value> {
        let id = req["id"].clone();
        self.send(&req).await?;
        loop {
            let r = self.recv().await?;
            if r["id"] == id {
                return Ok(r);
            }
        }
    }
}

struct Ctx {
    args: BenchArgs,
    manifest: Option<Value>,
    pids: std::sync::Mutex<HashSet<i32>>,
    latencies: std::sync::Mutex<Vec<Duration>>,
    errors: AtomicUsize,
    ok: AtomicU64,
    first_error: std::sync::Mutex<Option<String>>,
}

async fn connect(ctx: &Ctx) -> Result<Conn> {
    let (cmd, rest) = ctx.args.command.split_first().unwrap();
    if ctx.args.direct {
        let mut child = tokio::process::Command::new(cmd)
            .args(rest)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning {cmd}"))?;
        if let Some(pid) = child.id() {
            ctx.pids.lock().unwrap().insert(pid as i32);
        }
        let w = child.stdin.take().unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        return Ok(Conn::Direct { lines, w, _child: Box::new(child) });
    }
    let stream = UnixStream::connect(paths::socket()).await.context("connecting to daemon")?;
    let (rd, mut w) = stream.into_split();
    let hello = Hello::Session(SessionHello {
        proto: PROTO,
        name: ctx.args.name.clone(),
        command: cmd.clone(),
        args: rest.to_vec(),
        env: std::env::vars().collect(),
        cwd: std::env::current_dir()?.display().to_string(),
        manifest: ctx.manifest.clone(),
        force_session: ctx.args.isolate,
        pid: std::process::id(),
    });
    w.write_all(format!("{}\n", serde_json::to_string(&hello)?).as_bytes()).await?;
    Ok(Conn::Carpool { lines: BufReader::new(rd).lines(), w })
}

fn meta(stateless: bool) -> Value {
    if stateless {
        json!({"io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientCapabilities": {}})
    } else {
        json!({})
    }
}

async fn one_session(ctx: Arc<Ctx>, tool: String, arguments: Value) -> Result<()> {
    let mut c = connect(&ctx).await?;
    if !ctx.args.stateless {
        let init = c
            .call(json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{
                "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"carpool-bench","version":"1"}}}))
            .await?;
        if init.get("error").is_some() {
            bail!("initialize failed: {}", init["error"]);
        }
        c.send(&json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await?;
    }
    for i in 0..ctx.args.calls {
        let t = Instant::now();
        let r = c
            .call(json!({"jsonrpc":"2.0","id":i+1,"method":"tools/call","params":{
                "name":tool,"arguments":arguments,"_meta":meta(ctx.args.stateless)}}))
            .await?;
        let dt = t.elapsed();
        if r.get("error").is_some() || r.pointer("/result/isError") == Some(&json!(true)) {
            ctx.errors.fetch_add(1, Ordering::Relaxed);
            ctx.first_error.lock().unwrap().get_or_insert_with(|| r.to_string());
        } else {
            ctx.ok.fetch_add(1, Ordering::Relaxed);
            ctx.latencies.lock().unwrap().push(dt);
        }
    }
    if ctx.args.hold > 0 {
        tokio::time::sleep(Duration::from_secs(ctx.args.hold)).await;
    }
    Ok(())
}

async fn first_tool(ctx: &Ctx) -> Result<String> {
    let mut c = connect(ctx).await?;
    if !ctx.args.stateless {
        c.call(json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{
            "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"carpool-bench","version":"1"}}}))
            .await?;
        c.send(&json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await?;
    }
    let r = c.call(json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":meta(ctx.args.stateless)}})).await?;
    r.pointer("/result/tools/0/name").and_then(Value::as_str).map(String::from).context("server lists no tools")
}

fn pct(sorted: &[Duration], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i].as_secs_f64() * 1000.0
}

pub async fn run(args: BenchArgs) -> Result<()> {
    let manifest = match &args.manifest {
        Some(p) => Some(serde_json::from_str(&std::fs::read_to_string(p)?)?),
        None => None,
    };
    let arguments: Value = serde_json::from_str(&args.arguments).context("--arguments must be JSON")?;
    if !args.direct {
        tokio::task::spawn_blocking(crate::shim::connect_or_start).await??;
    }
    let ctx = Arc::new(Ctx {
        args,
        manifest,
        pids: Default::default(),
        latencies: Default::default(),
        errors: AtomicUsize::new(0),
        ok: AtomicU64::new(0),
        first_error: Default::default(),
    });
    let tool = match &ctx.args.tool {
        Some(t) => t.clone(),
        None => first_tool(&ctx).await?,
    };
    let mode = if ctx.args.direct { "direct (one process per session)" } else { "through carpool" };
    println!("bench: {} sessions × {} calls of `{tool}` — {mode}", ctx.args.sessions, ctx.args.calls);

    // Sample processes and memory while the load runs.
    let peak = Arc::new(std::sync::Mutex::new((0usize, 0u64)));
    let sampler = {
        let ctx = ctx.clone();
        let peak = peak.clone();
        tokio::spawn(async move {
            loop {
                let (procs, kb) = if ctx.args.direct {
                    let rss = crate::daemon::process::sample_rss().await;
                    let pids = ctx.pids.lock().unwrap().clone();
                    (pids.iter().filter(|p| rss.contains_key(p)).count(), pids.iter().filter_map(|p| rss.get(p)).sum())
                } else {
                    match status::fetch().await {
                        Ok(s) => (s.totals.server_processes, s.totals.rss_mb * 1024),
                        Err(_) => (0, 0),
                    }
                };
                {
                    let mut p = peak.lock().unwrap();
                    p.0 = p.0.max(procs);
                    p.1 = p.1.max(kb);
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        })
    };

    let started = Instant::now();
    let mut tasks = vec![];
    for _ in 0..ctx.args.sessions {
        let ctx2 = ctx.clone();
        let tool = tool.clone();
        let arguments = arguments.clone();
        tasks.push(tokio::spawn(async move {
            if let Err(e) = one_session(ctx2.clone(), tool, arguments).await {
                ctx2.errors.fetch_add(1, Ordering::Relaxed);
                ctx2.first_error.lock().unwrap().get_or_insert_with(|| format!("{e:#}"));
            }
        }));
        if ctx.args.ramp_ms > 0 {
            tokio::time::sleep(Duration::from_millis(ctx.args.ramp_ms)).await;
        }
    }
    for t in tasks {
        let _ = t.await;
    }
    let elapsed = started.elapsed();
    tokio::time::sleep(Duration::from_millis(350)).await;
    sampler.abort();

    let mut lat = ctx.latencies.lock().unwrap().clone();
    lat.sort();
    let ok = ctx.ok.load(Ordering::Relaxed);
    let (procs, kb) = *peak.lock().unwrap();
    println!("  calls ok      {ok}   errors {}", ctx.errors.load(Ordering::Relaxed));
    if let Some(e) = ctx.first_error.lock().unwrap().as_ref() {
        println!("  first error   {e:.300}");
    }
    println!("  wall time     {:.2}s   throughput {:.0} calls/s", elapsed.as_secs_f64(), ok as f64 / elapsed.as_secs_f64());
    println!("  latency ms    p50 {:.1}   p95 {:.1}   p99 {:.1}   max {:.1}", pct(&lat, 0.5), pct(&lat, 0.95), pct(&lat, 0.99), pct(&lat, 1.0));
    println!("  peak servers  {procs} processes   {:.0} MB RSS", kb as f64 / 1024.0);
    Ok(())
}
