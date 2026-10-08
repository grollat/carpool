//! The per-user daemon: accepts shim connections on a Unix socket, resolves
//! each server's manifest, and either runs a dedicated instance (session
//! isolation) or attaches the session to a shared, autoscaled pool.

pub mod dedicated;
pub mod pool;
pub mod process;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

use crate::discovery::{Discovery, Resolved};
use crate::identity::Identity;
use crate::manifest::Isolation;
use crate::wire::{self, Hello, SessionHello};
use crate::{brand, jsonrpc, paths};
use pool::{Mode, Pool, PoolConfig, PoolEvent, PoolSnapshot};
use process::{LaunchSpec, BASELINE_ENV};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    /// Machine-wide cap on shared instances across all pools.
    pub max_shared_instances: usize,
    /// Look up manifests published by a package's own repository / npm metadata.
    pub remote_discovery: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config { max_shared_instances: 256, remote_discovery: true }
    }
}

impl Config {
    pub fn load() -> Config {
        std::fs::read_to_string(paths::config_file())
            .ok()
            .and_then(|t| toml::from_str(&t).map_err(|e| eprintln!("ignoring invalid config.toml: {e}")).ok())
            .unwrap_or_default()
    }
}

pub struct DedicatedInfo {
    pub name: String,
    pub identity: String,
    pub pid: i32,
    pub source: String,
    pub started: Instant,
}

pub struct Daemon {
    pub config: Config,
    pub discovery: Discovery,
    pub pools: Mutex<HashMap<String, mpsc::UnboundedSender<PoolEvent>>>,
    pub dedicated: Mutex<HashMap<u64, DedicatedInfo>>,
    /// Resident KiB per process group, refreshed every few seconds.
    pub rss: Mutex<HashMap<i32, u64>>,
    pub shared_instances: AtomicUsize,
    /// Base keys of servers known not to speak the stateless protocol (they
    /// reject or ignore `server/discover`). Persisted so probes are answered
    /// instantly after a restart.
    pub legacy_only: Mutex<HashSet<String>>,
    next_sid: AtomicU64,
    started: Instant,
    shutdown: tokio::sync::Notify,
}

// ---------------------------------------------------------------- groups

fn groups() -> &'static Mutex<HashSet<i32>> {
    static G: OnceLock<Mutex<HashSet<i32>>> = OnceLock::new();
    G.get_or_init(Default::default)
}

pub fn track_group(pgid: i32) {
    groups().lock().unwrap().insert(pgid);
}

pub fn untrack_group(pgid: i32) {
    groups().lock().unwrap().remove(&pgid);
}

fn kill_all_groups(signal: i32) {
    for pgid in groups().lock().unwrap().iter() {
        process::killpg(*pgid, signal);
    }
}

// ---------------------------------------------------------- legacy probe

const LEGACY_TTL_SECS: u64 = 24 * 3600;

fn legacy_only_path() -> PathBuf {
    paths::cache().join("legacy-only.json")
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn load_legacy_only() -> HashSet<String> {
    let map: HashMap<String, u64> =
        std::fs::read_to_string(legacy_only_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let now = unix_now();
    map.into_iter().filter(|(_, at)| now.saturating_sub(*at) < LEGACY_TTL_SECS).map(|(k, _)| k).collect()
}

impl Daemon {
    pub fn mark_legacy_only(&self, base_key: &str) {
        if !self.legacy_only.lock().unwrap().insert(base_key.to_string()) {
            return;
        }
        let mut map: HashMap<String, u64> =
            std::fs::read_to_string(legacy_only_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        map.insert(base_key.to_string(), unix_now());
        let _ = std::fs::write(legacy_only_path(), serde_json::to_string(&map).unwrap_or_default());
    }
}

// ------------------------------------------------------------------ logs

pub fn server_log(name: &str) -> PathBuf {
    paths::logs().join("servers").join(format!("{}.log", paths::sanitize(name)))
}

pub async fn open_server_log(name: &str) -> Option<tokio::fs::File> {
    let path = server_log(name);
    tokio::fs::create_dir_all(path.parent()?).await.ok()?;
    tokio::fs::OpenOptions::new().create(true).append(true).open(path).await.ok()
}

pub async fn log_stderr(stderr: tokio::process::ChildStderr, name: String, pid: i32) {
    let mut lines = BufReader::new(stderr).lines();
    let mut log = open_server_log(&name).await;
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(f) = log.as_mut() {
            let _ = f.write_all(format!("[{pid}] {line}\n").as_bytes()).await;
        }
    }
}

// ------------------------------------------------------------------ main

pub async fn run() -> Result<()> {
    paths::ensure()?;
    let lock = std::fs::File::create(paths::lock_file())?;
    use std::os::fd::AsRawFd;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("another {} daemon is already running", brand::NAME);
    }
    let sock = paths::socket();
    if sock.exists() {
        if UnixStream::connect(&sock).await.is_ok() {
            bail!("another {} daemon is already listening on {}", brand::NAME, sock.display());
        }
        std::fs::remove_file(&sock).ok();
    }
    let listener = UnixListener::bind(&sock).with_context(|| format!("binding {}", sock.display()))?;
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;

    let config = Config::load();
    let daemon = Arc::new(Daemon {
        discovery: Discovery::new(config.remote_discovery),
        config,
        pools: Mutex::new(HashMap::new()),
        dedicated: Mutex::new(HashMap::new()),
        rss: Mutex::new(HashMap::new()),
        shared_instances: AtomicUsize::new(0),
        legacy_only: Mutex::new(load_legacy_only()),
        next_sid: AtomicU64::new(1),
        started: Instant::now(),
        shutdown: tokio::sync::Notify::new(),
    });
    tracing::info!(pid = std::process::id(), socket = %sock.display(), version = brand::VERSION, "daemon listening");

    let d = daemon.clone();
    tokio::spawn(async move {
        loop {
            let sample = process::sample_rss().await;
            *d.rss.lock().unwrap() = sample;
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                if let Ok((stream, _)) = accepted {
                    let d = daemon.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_conn(d, stream).await {
                            tracing::debug!("connection ended: {e:#}");
                        }
                    });
                }
            }
            _ = daemon.shutdown.notified() => break,
            _ = tokio::signal::ctrl_c() => break,
            _ = term.recv() => break,
        }
    }

    tracing::info!("shutting down");
    kill_all_groups(libc::SIGTERM);
    tokio::time::sleep(Duration::from_millis(500)).await;
    kill_all_groups(libc::SIGKILL);
    std::fs::remove_file(&sock).ok();
    drop(lock);
    Ok(())
}

async fn handle_conn(daemon: Arc<Daemon>, stream: UnixStream) -> Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::with_capacity(64 * 1024, rd).lines();
    let first = lines.next_line().await?.context("connection closed before hello")?;
    let hello: Hello = serde_json::from_str(&first).context("invalid hello")?;
    match hello {
        Hello::Status => {
            let snap = daemon.snapshot().await;
            wr.write_all(serde_json::to_string(&snap)?.as_bytes()).await?;
            wr.write_all(b"\n").await?;
        }
        Hello::Shutdown => {
            wr.write_all(b"{\"ok\":true}\n").await?;
            daemon.shutdown.notify_one();
        }
        Hello::Session(h) => session(daemon, h, lines, wr).await,
    }
    Ok(())
}

// --------------------------------------------------------------- session

fn hash_of(v: &impl Hash) -> String {
    let mut h = DefaultHasher::new();
    v.hash(&mut h);
    format!("{:016x}", h.finish())
}

async fn session(daemon: Arc<Daemon>, h: SessionHello, lines: Lines<BufReader<OwnedReadHalf>>, mut out: OwnedWriteHalf) {
    let sid = daemon.next_sid.fetch_add(1, Ordering::Relaxed);
    let cwd = PathBuf::from(&h.cwd);
    let identity = Identity::from_command(&h.command, &h.args, &cwd);
    let resolved = daemon.discovery.resolve(&h, &identity);
    let program = process::resolve_program(&h.command, &h.env, &cwd);
    tracing::info!(
        sid, server = %h.name, identity = %identity, isolation = %resolved.manifest.isolation,
        source = %resolved.source, "session opened"
    );

    if h.proto != wire::PROTO {
        let msg = format!("[{}] shim/daemon version mismatch; run `{} stop`", brand::NAME, brand::NAME);
        let _ = out.write_all(wire::stderr_frame(&msg).as_bytes()).await;
    }

    match resolved.manifest.isolation {
        Isolation::Session => {
            // Exactly what the client would have run: full env, its cwd.
            let spec = LaunchSpec { program, args: h.args.clone(), env: h.env.clone(), cwd };
            dedicated::run(daemon, sid, h.name, identity, resolved.source.to_string(), spec, vec![], lines, out).await;
        }
        Isolation::Shared => shared_session(daemon, sid, h, identity, resolved, program, lines, out).await,
    }
}

/// The launch spec for a shared instance: only the baseline and declared env
/// vars are passed, and they (with command, args and cwd) form the partition.
fn shared_spec(h: &SessionHello, resolved: &Resolved, program: PathBuf) -> LaunchSpec {
    let m = &resolved.manifest;
    let env: BTreeMap<String, String> = h
        .env
        .iter()
        .filter(|(k, _)| BASELINE_ENV.contains(&k.as_str()) || m.env_declared(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let cwd = if m.partition.cwd { PathBuf::from(&h.cwd) } else { paths::user_home() };
    LaunchSpec { program, args: h.args.clone(), env, cwd }
}

#[allow(clippy::too_many_arguments)]
async fn shared_session(
    daemon: Arc<Daemon>,
    sid: u64,
    h: SessionHello,
    identity: Identity,
    resolved: Resolved,
    program: PathBuf,
    mut lines: Lines<BufReader<OwnedReadHalf>>,
    mut out: OwnedWriteHalf,
) {
    let spec = shared_spec(&h, &resolved, program);
    let base_key = hash_of(&(&spec, format!("{:?}", resolved.manifest.scaling)));

    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if out.write_all(frame.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    let mut current: Option<(Mode, mpsc::UnboundedSender<PoolEvent>)> = None;
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let err = json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":format!("parse error: {e}")}});
                let _ = tx.send(wire::msg_frame(&err.to_string()));
                continue;
            }
        };
        let method = jsonrpc::method(&msg).unwrap_or_default().to_string();
        let is_init = method == "initialize" && jsonrpc::is_request(&msg);
        let needs_pool = match &current {
            None => jsonrpc::is_request(&msg),
            // A client that probed with server/discover and fell back to initialize.
            Some((Mode::Stateless, _)) => is_init,
            Some(_) => false,
        };
        if !needs_pool {
            if let Some((_, pool)) = &current {
                let _ = pool.send(PoolEvent::FromSession { sid, msg });
            }
            continue;
        }
        if !is_init && method == "server/discover" && daemon.legacy_only.lock().unwrap().contains(&base_key) {
            let err = jsonrpc::error(&msg["id"], jsonrpc::METHOD_NOT_FOUND, "Method not found: server/discover");
            let _ = tx.send(wire::msg_frame(&err.to_string()));
            continue;
        }
        if let Some((old_mode, old)) = current.take() {
            // The client probed with server/discover and gave up: the server is
            // pre-2026 (some SDKs ignore unknown methods instead of erroring).
            if old_mode == Mode::Stateless && is_init {
                daemon.mark_legacy_only(&base_key);
            }
            let _ = old.send(PoolEvent::Detach { sid });
        }
        let (mode, init_params, key) = if is_init {
            let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
            let proto = params.get("protocolVersion").cloned().unwrap_or(Value::Null);
            // Same client software and version share; anything else gets its own pool,
            // so one client's handshake never shapes another client's backend.
            let client = params.get("clientInfo").cloned().unwrap_or(Value::Null);
            // Backends never see the client's capabilities: a shared server must not
            // send requests that cannot be attributed to one session.
            let init = json!({
                "protocolVersion": proto,
                "capabilities": {},
                "clientInfo": params.get("clientInfo").cloned().unwrap_or_else(|| json!({"name": brand::NAME, "version": brand::VERSION})),
            });
            (Mode::Legacy, Some(init), hash_of(&(&base_key, "legacy", proto.to_string(), client.to_string())))
        } else {
            (Mode::Stateless, None, hash_of(&(&base_key, "stateless")))
        };
        let pool = {
            let mut pools = daemon.pools.lock().unwrap();
            let existing = pools.get(&key).filter(|p| !p.is_closed()).cloned();
            let pool = existing.unwrap_or_else(|| {
                let cfg = PoolConfig {
                    key: key.clone(),
                    base_key: base_key.clone(),
                    name: h.name.clone(),
                    identity: identity.clone(),
                    spec: spec.clone(),
                    manifest: resolved.manifest.clone(),
                    source: resolved.source.to_string(),
                    mode,
                    init_params,
                };
                let p = Pool::start(cfg, daemon.clone());
                pools.insert(key.clone(), p.clone());
                p
            });
            let _ = pool.send(PoolEvent::Attach { sid, tx: tx.clone(), first: msg });
            pool
        };
        current = Some((mode, pool));
    }
    if let Some((_, pool)) = current {
        let _ = pool.send(PoolEvent::Detach { sid });
    }
    drop(tx);
    let _ = writer.await;
    tracing::info!(sid, "session closed");
}

// ---------------------------------------------------------------- status

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DedicatedSnapshot {
    pub name: String,
    pub identity: String,
    pub pid: i32,
    pub rss_kb: u64,
    pub age_secs: u64,
    pub manifest_source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Totals {
    pub sessions: usize,
    pub server_processes: usize,
    pub rss_mb: u64,
    /// Processes the same sessions would have spawned without Carpool.
    pub processes_without_carpool: usize,
    /// Estimated memory those extra processes would have used.
    pub est_rss_saved_mb: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: String,
    pub pid: u32,
    pub uptime_secs: u64,
    pub socket: String,
    pub totals: Totals,
    pub pools: Vec<PoolSnapshotOwned>,
    pub dedicated: Vec<DedicatedSnapshot>,
}

/// PoolSnapshot round-trips through JSON for the CLI.
pub type PoolSnapshotOwned = serde_json::Value;

impl Daemon {
    async fn snapshot(&self) -> Snapshot {
        *self.rss.lock().unwrap() = process::sample_rss().await;
        let handles: Vec<_> = self.pools.lock().unwrap().values().cloned().collect();
        let mut pools: Vec<PoolSnapshot> = vec![];
        for h in handles {
            let (tx, rx) = oneshot::channel();
            if h.send(PoolEvent::Snapshot(tx)).is_ok() {
                if let Ok(Ok(s)) = tokio::time::timeout(Duration::from_secs(2), rx).await {
                    pools.push(s);
                }
            }
        }
        pools.sort_by(|a, b| a.name.cmp(&b.name));
        let rss = self.rss.lock().unwrap().clone();
        let mut dedicated: Vec<DedicatedSnapshot> = self
            .dedicated
            .lock()
            .unwrap()
            .values()
            .map(|d| DedicatedSnapshot {
                name: d.name.clone(),
                identity: d.identity.clone(),
                pid: d.pid,
                rss_kb: rss.get(&d.pid).copied().unwrap_or(0),
                age_secs: d.started.elapsed().as_secs(),
                manifest_source: d.source.clone(),
            })
            .collect();
        dedicated.sort_by(|a, b| a.name.cmp(&b.name).then(a.pid.cmp(&b.pid)));

        let mut t = Totals { sessions: dedicated.len(), server_processes: dedicated.len(), ..Default::default() };
        t.processes_without_carpool = dedicated.len();
        let mut rss_kb: u64 = dedicated.iter().map(|d| d.rss_kb).sum();
        let mut saved_kb = 0;
        for p in &pools {
            let n = p.instances.len();
            let kb: u64 = p.instances.iter().map(|i| i.rss_kb).sum();
            t.sessions += p.sessions;
            t.server_processes += n;
            t.processes_without_carpool += p.sessions;
            rss_kb += kb;
            if n > 0 && p.sessions > n {
                saved_kb += kb / n as u64 * (p.sessions - n) as u64;
            }
        }
        t.rss_mb = rss_kb / 1024;
        t.est_rss_saved_mb = saved_kb / 1024;

        Snapshot {
            version: brand::VERSION.to_string(),
            pid: std::process::id(),
            uptime_secs: self.started.elapsed().as_secs(),
            socket: paths::socket().display().to_string(),
            totals: t,
            pools: pools.into_iter().map(|p| serde_json::to_value(p).unwrap()).collect(),
            dedicated,
        }
    }
}

/// Client side of `status` / `stop`.
pub async fn request(hello: &Hello) -> Result<Value> {
    let stream = UnixStream::connect(paths::socket()).await.context("daemon is not running")?;
    let (rd, mut wr) = stream.into_split();
    wr.write_all(serde_json::to_string(hello)?.as_bytes()).await?;
    wr.write_all(b"\n").await?;
    let mut lines = BufReader::new(rd).lines();
    let line = lines.next_line().await?.context("daemon closed the connection")?;
    Ok(serde_json::from_str(&line)?)
}
