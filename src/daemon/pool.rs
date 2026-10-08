//! A pool of interchangeable instances of one shared MCP server, serving many
//! client sessions. One task owns all pool state; sessions, instance readers
//! and a ticker talk to it through a single channel, so there are no locks on
//! the hot path.
//!
//! Multiplexing rules (see spec/SPEC.md, "Runtime behaviour"):
//! * request ids are rewritten to pool-unique integers and mapped back;
//! * progress tokens are rewritten the same way;
//! * `notifications/cancelled` is translated to the backend id;
//! * resource subscriptions are reference counted;
//! * list_changed / logging notifications are broadcast to every session;
//! * legacy (initialize-based) backends are initialized once by the runtime,
//!   with no client capabilities, so they never send sampling/elicitation/roots
//!   requests that could not be attributed to a session.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use super::process::{self, LaunchSpec};
use super::Daemon;
use crate::identity::Identity;
use crate::jsonrpc::{self, id_key};
use crate::manifest::Manifest;
use crate::{brand, wire};

pub type SessionTx = mpsc::UnboundedSender<String>;
pub type SessionId = u64;
type InstanceId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Clients and backend speak the `initialize` handshake (<= 2025-11-25).
    Legacy,
    /// Stateless protocol (>= 2026-07-28): every request is self-describing.
    Stateless,
}

pub struct PoolConfig {
    pub key: String,
    pub base_key: String,
    pub name: String,
    pub identity: Identity,
    pub spec: LaunchSpec,
    pub manifest: Manifest,
    pub source: String,
    pub mode: Mode,
    /// Legacy only: params used to initialize every backend instance.
    pub init_params: Option<Value>,
}

pub enum PoolEvent {
    Attach { sid: SessionId, tx: SessionTx, first: Value },
    FromSession { sid: SessionId, msg: Value },
    Detach { sid: SessionId },
    FromInstance { iid: InstanceId, line: String },
    InstanceExited { iid: InstanceId, code: Option<i32> },
    Tick,
    Snapshot(oneshot::Sender<PoolSnapshot>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum State {
    Starting,
    Ready,
    Draining,
}

struct Instance {
    pid: i32,
    state: State,
    stdin: Option<mpsc::UnboundedSender<String>>,
    stop: Option<oneshot::Sender<()>>,
    in_flight: u32,
    served: u64,
    started: Instant,
    idle_since: Instant,
    init_id: Option<i64>,
}

struct Pending {
    sid: SessionId,
    orig_id: Value,
    iid: InstanceId,
    method: String,
    progress: Option<String>,
    subscribe_uri: Option<String>,
}

struct Session {
    tx: SessionTx,
    initialized: bool,
}

struct Queued {
    sid: SessionId,
    msg: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstanceSnapshot {
    pub pid: i32,
    pub state: &'static str,
    pub in_flight: u32,
    pub served: u64,
    pub rss_kb: u64,
    pub age_secs: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PoolSnapshot {
    pub name: String,
    pub identity: String,
    pub key: String,
    pub mode: Mode,
    pub manifest_source: String,
    pub reason: Option<String>,
    pub sessions: usize,
    pub queued: usize,
    pub max_instances: u32,
    pub max_concurrent_requests: u32,
    pub instances: Vec<InstanceSnapshot>,
}

pub struct Pool {
    cfg: PoolConfig,
    daemon: Arc<Daemon>,
    tx: mpsc::UnboundedSender<PoolEvent>,
    instances: BTreeMap<InstanceId, Instance>,
    sessions: HashMap<SessionId, Session>,
    pending: HashMap<i64, Pending>,
    by_session: HashMap<(SessionId, String), i64>,
    progress: HashMap<String, (SessionId, Value)>,
    /// uri -> (subscribed sessions, instance holding the backend subscription)
    subscriptions: HashMap<String, (HashSet<SessionId>, Option<InstanceId>)>,
    queue: VecDeque<Queued>,
    init_result: Option<Value>,
    waiting_init: Vec<(SessionId, Value)>,
    next_id: i64,
    next_iid: InstanceId,
    failures: u32,
    retry_at: Option<Instant>,
    empty_since: Option<Instant>,
}

const MAX_FAILURES: u32 = 3;

impl Pool {
    pub fn start(cfg: PoolConfig, daemon: Arc<Daemon>) -> mpsc::UnboundedSender<PoolEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        let pool = Pool {
            cfg,
            daemon,
            tx: tx.clone(),
            instances: BTreeMap::new(),
            sessions: HashMap::new(),
            pending: HashMap::new(),
            by_session: HashMap::new(),
            progress: HashMap::new(),
            subscriptions: HashMap::new(),
            queue: VecDeque::new(),
            init_result: None,
            waiting_init: vec![],
            next_id: 1,
            next_iid: 1,
            failures: 0,
            retry_at: None,
            empty_since: None,
        };
        let ticker = tx.clone();
        tokio::spawn(async move {
            let mut every = tokio::time::interval(Duration::from_millis(500));
            loop {
                every.tick().await;
                if ticker.send(PoolEvent::Tick).is_err() {
                    break;
                }
            }
        });
        tokio::spawn(pool.run(rx));
        tx
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<PoolEvent>) {
        tracing::info!(pool = %self.cfg.name, key = %self.cfg.key, mode = ?self.cfg.mode, "pool created");
        while let Some(ev) = rx.recv().await {
            self.handle(ev);
            if self.is_empty() {
                let since = *self.empty_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= Duration::from_secs(2) && self.try_retire(&mut rx) {
                    tracing::info!(pool = %self.cfg.name, "pool retired");
                    return;
                }
            } else {
                self.empty_since = None;
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.sessions.is_empty() && self.instances.is_empty() && self.queue.is_empty()
    }

    /// Remove this pool from the registry, unless an Attach raced in. Attach
    /// is only ever sent while holding the registry lock, so draining the
    /// channel under the same lock is race-free.
    fn try_retire(&mut self, rx: &mut mpsc::UnboundedReceiver<PoolEvent>) -> bool {
        let daemon = self.daemon.clone();
        let mut pools = daemon.pools.lock().unwrap();
        let mut late = vec![];
        while let Ok(ev) = rx.try_recv() {
            late.push(ev);
        }
        let attached = late.iter().any(|e| matches!(e, PoolEvent::Attach { .. }));
        if !attached {
            pools.remove(&self.cfg.key);
            drop(pools);
            for ev in late {
                if let PoolEvent::Snapshot(reply) = ev {
                    let _ = reply.send(self.snapshot());
                }
            }
            return true;
        }
        drop(pools);
        for ev in late {
            self.handle(ev);
        }
        false
    }

    fn handle(&mut self, ev: PoolEvent) {
        match ev {
            PoolEvent::Attach { sid, tx, first } => {
                self.sessions.insert(sid, Session { tx, initialized: false });
                self.on_session(sid, first);
            }
            PoolEvent::FromSession { sid, msg } => self.on_session(sid, msg),
            PoolEvent::Detach { sid } => self.detach(sid),
            PoolEvent::FromInstance { iid, line } => self.on_instance(iid, &line),
            PoolEvent::InstanceExited { iid, code } => self.instance_exited(iid, code),
            PoolEvent::Tick => self.tick(),
            PoolEvent::Snapshot(reply) => {
                let _ = reply.send(self.snapshot());
            }
        }
        self.pump();
    }

    // ----------------------------------------------------------- sessions

    fn send(&self, sid: SessionId, msg: &Value) {
        if let Some(s) = self.sessions.get(&sid) {
            let _ = s.tx.send(wire::msg_frame(&msg.to_string()));
        }
    }

    fn on_session(&mut self, sid: SessionId, msg: Value) {
        if jsonrpc::is_request(&msg) {
            let id = msg["id"].clone();
            match jsonrpc::method(&msg).unwrap_or_default() {
                "initialize" if self.cfg.mode == Mode::Legacy => match &self.init_result {
                    Some(r) => {
                        let r = r.clone();
                        self.send(sid, &jsonrpc::result(&id, r));
                    }
                    None => self.waiting_init.push((sid, id)),
                },
                "ping" if self.cfg.mode == Mode::Legacy => self.send(sid, &jsonrpc::result(&id, json!({}))),
                // Log level is process-global; one session must not change it for the others.
                "logging/setLevel" => self.send(sid, &jsonrpc::result(&id, json!({}))),
                "resources/subscribe" => self.subscribe(sid, msg),
                "resources/unsubscribe" => self.unsubscribe(sid, msg),
                _ => self.queue.push_back(Queued { sid, msg }),
            }
        } else if jsonrpc::is_notification(&msg) {
            match jsonrpc::method(&msg).unwrap_or_default() {
                "notifications/initialized" => {
                    if let Some(s) = self.sessions.get_mut(&sid) {
                        s.initialized = true;
                    }
                }
                "notifications/cancelled" => {
                    if let Some(orig) = msg.pointer("/params/requestId") {
                        self.cancel(sid, orig.clone(), msg.pointer("/params/reason").cloned());
                    }
                }
                // Roots/progress from the client have no meaning for a shared server.
                _ => {}
            }
        }
        // Responses from the client answer server->client requests, which a
        // shared pool never forwards; drop them.
    }

    fn subscribe(&mut self, sid: SessionId, msg: Value) {
        let id = msg["id"].clone();
        let Some(uri) = msg.pointer("/params/uri").and_then(Value::as_str).map(String::from) else {
            self.queue.push_back(Queued { sid, msg });
            return;
        };
        let entry = self.subscriptions.entry(uri.clone()).or_insert_with(|| (HashSet::new(), None));
        let first = entry.0.is_empty() && entry.1.is_none();
        entry.0.insert(sid);
        if first {
            self.queue.push_back(Queued { sid, msg });
        } else {
            self.send(sid, &jsonrpc::result(&id, json!({})));
        }
    }

    fn unsubscribe(&mut self, sid: SessionId, msg: Value) {
        let id = msg["id"].clone();
        let uri = msg.pointer("/params/uri").and_then(Value::as_str).unwrap_or_default().to_string();
        let last = match self.subscriptions.get_mut(&uri) {
            Some((subs, _)) => {
                subs.remove(&sid);
                subs.is_empty()
            }
            None => false,
        };
        if last {
            let holder = self.subscriptions.remove(&uri).and_then(|(_, iid)| iid);
            if let Some(iid) = holder {
                self.forward_internal(iid, "resources/unsubscribe", json!({"uri": uri}));
            }
        }
        self.send(sid, &jsonrpc::result(&id, json!({})));
    }

    fn cancel(&mut self, sid: SessionId, orig: Value, reason: Option<Value>) {
        let key = (sid, id_key(&orig));
        if let Some(bid) = self.by_session.remove(&key) {
            if let Some(p) = self.pending.remove(&bid) {
                self.release(p.iid, &p);
                let mut params = json!({"requestId": bid});
                if let Some(r) = reason {
                    params["reason"] = r;
                }
                self.write(p.iid, &jsonrpc::notification("notifications/cancelled", params));
            }
        } else {
            self.queue.retain(|q| !(q.sid == sid && q.msg.get("id") == Some(&orig)));
        }
    }

    fn detach(&mut self, sid: SessionId) {
        self.sessions.remove(&sid);
        self.waiting_init.retain(|(s, _)| *s != sid);
        self.queue.retain(|q| q.sid != sid);
        let mine: Vec<i64> = self.pending.iter().filter(|(_, p)| p.sid == sid).map(|(b, _)| *b).collect();
        for bid in mine {
            if let Some(p) = self.pending.remove(&bid) {
                self.by_session.remove(&(sid, id_key(&p.orig_id)));
                self.release(p.iid, &p);
                let note = jsonrpc::notification(
                    "notifications/cancelled",
                    json!({"requestId": bid, "reason": "client session closed"}),
                );
                self.write(p.iid, &note);
            }
        }
        let uris: Vec<String> = self.subscriptions.keys().cloned().collect();
        for uri in uris {
            let (subs, holder) = self.subscriptions.get_mut(&uri).unwrap();
            if subs.remove(&sid) && subs.is_empty() {
                let holder = *holder;
                self.subscriptions.remove(&uri);
                if let Some(iid) = holder {
                    self.forward_internal(iid, "resources/unsubscribe", json!({"uri": uri}));
                }
            }
        }
    }

    // ---------------------------------------------------------- dispatch

    /// Route queued requests to instances: fill the oldest instance up to the
    /// target load first (bin packing, so surplus instances go idle and can be
    /// reaped), then spill to the least-loaded instance below the hard cap.
    fn pump(&mut self) {
        while !self.queue.is_empty() {
            let Some(iid) = self.pick() else { break };
            let q = self.queue.pop_front().unwrap();
            if !self.sessions.contains_key(&q.sid) {
                continue;
            }
            self.dispatch(iid, q);
        }
        self.autoscale();
    }

    fn pick(&self) -> Option<InstanceId> {
        let s = &self.cfg.manifest.scaling;
        let ready = || self.instances.iter().filter(|(_, i)| i.state == State::Ready && i.stdin.is_some());
        if let Some((iid, _)) = ready().find(|(_, i)| i.in_flight < s.target()) {
            return Some(*iid);
        }
        ready().filter(|(_, i)| i.in_flight < s.max_concurrent_requests).min_by_key(|(_, i)| i.in_flight).map(|(iid, _)| *iid)
    }

    fn dispatch(&mut self, iid: InstanceId, q: Queued) {
        let Queued { sid, mut msg } = q;
        let bid = self.next_id;
        self.next_id += 1;
        let orig_id = msg["id"].clone();
        let method = jsonrpc::method(&msg).unwrap_or_default().to_string();
        let mut progress = None;
        if let Some(tok) = msg.pointer("/params/_meta/progressToken").cloned() {
            let new_tok = format!("{}-p{bid}", brand::NAME);
            msg["params"]["_meta"]["progressToken"] = json!(new_tok);
            self.progress.insert(new_tok.clone(), (sid, tok));
            progress = Some(new_tok);
        }
        let subscribe_uri = (method == "resources/subscribe")
            .then(|| msg.pointer("/params/uri").and_then(Value::as_str).map(String::from))
            .flatten();
        msg["id"] = json!(bid);
        self.by_session.insert((sid, id_key(&orig_id)), bid);
        self.pending.insert(bid, Pending { sid, orig_id, iid, method, progress, subscribe_uri });
        if let Some(inst) = self.instances.get_mut(&iid) {
            inst.in_flight += 1;
        }
        self.write(iid, &msg);
    }

    fn release(&mut self, iid: InstanceId, p: &Pending) {
        if let Some(tok) = &p.progress {
            self.progress.remove(tok);
        }
        if let Some(inst) = self.instances.get_mut(&iid) {
            inst.in_flight = inst.in_flight.saturating_sub(1);
            if inst.in_flight == 0 {
                inst.idle_since = Instant::now();
            }
        }
    }

    fn write(&self, iid: InstanceId, msg: &Value) {
        if let Some(stdin) = self.instances.get(&iid).and_then(|i| i.stdin.as_ref()) {
            let _ = stdin.send(msg.to_string());
        }
    }

    /// A request the runtime itself issues (its response is dropped).
    fn forward_internal(&mut self, iid: InstanceId, method: &str, params: Value) {
        let bid = self.next_id;
        self.next_id += 1;
        self.write(iid, &json!({"jsonrpc": "2.0", "id": bid, "method": method, "params": params}));
    }

    // --------------------------------------------------------- instances

    fn on_instance(&mut self, iid: InstanceId, line: &str) {
        let Ok(msg) = serde_json::from_str::<Value>(line) else {
            tracing::debug!(pool = %self.cfg.name, "ignoring non-JSON stdout line: {line:.200}");
            return;
        };
        if jsonrpc::is_response(&msg) {
            self.instance_response(iid, msg);
        } else if jsonrpc::is_request(&msg) {
            // Server->client request. Shared instances are initialized without
            // client capabilities, so only ping is legitimate here.
            let id = msg["id"].clone();
            let reply = if jsonrpc::method(&msg) == Some("ping") {
                jsonrpc::result(&id, json!({}))
            } else {
                jsonrpc::error(
                    &id,
                    jsonrpc::METHOD_NOT_FOUND,
                    format!("{} shares this server between sessions; server-to-client requests are not available", brand::DISPLAY),
                )
            };
            self.write(iid, &reply);
        } else if jsonrpc::is_notification(&msg) {
            self.instance_notification(msg);
        }
    }

    fn instance_response(&mut self, iid: InstanceId, mut msg: Value) {
        let Some(bid) = msg["id"].as_i64() else { return };
        let is_init = self.instances.get(&iid).is_some_and(|i| i.init_id == Some(bid));
        if is_init {
            self.instance_initialized(iid, msg);
            return;
        }
        let Some(p) = self.pending.remove(&bid) else { return };
        self.by_session.remove(&(p.sid, id_key(&p.orig_id)));
        self.release(iid, &p);
        if let Some(inst) = self.instances.get_mut(&iid) {
            inst.served += 1;
            if let Some(max) = self.cfg.manifest.scaling.max_requests_per_instance {
                if inst.served >= max && inst.state == State::Ready {
                    tracing::info!(pool = %self.cfg.name, pid = inst.pid, "recycling after {max} requests");
                    inst.state = State::Draining;
                }
            }
        }
        if let Some(uri) = &p.subscribe_uri {
            if msg.get("result").is_some() {
                if let Some(entry) = self.subscriptions.get_mut(uri) {
                    entry.1 = Some(iid);
                }
            }
        }
        if p.method == "server/discover" {
            match msg.get("result") {
                Some(result) => self.daemon.discovery.observe(&self.cfg.identity, result),
                None => self.daemon.mark_legacy_only(&self.cfg.base_key),
            }
        }
        msg["id"] = p.orig_id;
        self.send(p.sid, &msg);
    }

    fn instance_initialized(&mut self, iid: InstanceId, msg: Value) {
        let Some(inst) = self.instances.get_mut(&iid) else { return };
        inst.init_id = None;
        if let Some(result) = msg.get("result") {
            inst.state = State::Ready;
            inst.idle_since = Instant::now();
            let _ = inst.stdin.as_ref().map(|s| s.send(jsonrpc::notification("notifications/initialized", json!({})).to_string()));
            self.failures = 0;
            self.daemon.discovery.observe(&self.cfg.identity, result);
            if self.init_result.is_none() {
                self.init_result = Some(result.clone());
            }
            let r = self.init_result.clone().unwrap();
            for (sid, id) in std::mem::take(&mut self.waiting_init) {
                self.send(sid, &jsonrpc::result(&id, r.clone()));
            }
        } else {
            let err = msg.get("error").cloned().unwrap_or(Value::Null);
            tracing::warn!(pool = %self.cfg.name, "initialize failed: {err}");
            self.stop_instance(iid);
            self.note_failure(format!("server rejected initialize: {err}"));
        }
    }

    fn instance_notification(&mut self, mut msg: Value) {
        let method = jsonrpc::method(&msg).unwrap_or_default().to_string();
        match method.as_str() {
            "notifications/progress" => {
                let tok = msg.pointer("/params/progressToken").and_then(Value::as_str).map(String::from);
                if let Some((sid, orig)) = tok.and_then(|t| self.progress.get(&t).cloned()) {
                    msg["params"]["progressToken"] = orig;
                    self.send(sid, &msg);
                }
            }
            "notifications/resources/updated" => {
                let uri = msg.pointer("/params/uri").and_then(Value::as_str).unwrap_or_default();
                if let Some((subs, _)) = self.subscriptions.get(uri) {
                    for sid in subs.clone() {
                        self.send(sid, &msg);
                    }
                }
            }
            "notifications/cancelled" => {}
            _ => {
                let legacy = self.cfg.mode == Mode::Legacy;
                for (sid, s) in &self.sessions {
                    if !legacy || s.initialized {
                        self.send(*sid, &msg);
                    }
                }
            }
        }
    }

    fn spawn_instance(&mut self) {
        let iid = self.next_iid;
        self.next_iid += 1;
        let mut child = match process::spawn(&self.cfg.spec) {
            Ok(c) => c,
            Err(e) => {
                self.note_failure(format!("failed to start {}: {e}", self.cfg.spec.program.display()));
                return;
            }
        };
        self.daemon.shared_instances.fetch_add(1, Ordering::Relaxed);
        let pid = child.id().map(|p| p as i32).unwrap_or(0);
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (in_tx, mut in_rx) = mpsc::unbounded_channel::<String>();
        let (stop_tx, stop_rx) = oneshot::channel();

        tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(line) = in_rx.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err() || stdin.write_all(b"\n").await.is_err() {
                    break;
                }
                let _ = stdin.flush().await;
            }
        });
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if tx.send(PoolEvent::FromInstance { iid, line }).is_err() {
                    break;
                }
            }
        });
        tokio::spawn(super::log_stderr(stderr, self.cfg.name.clone(), pid));
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let code = process::supervise(child, stop_rx).await;
            let _ = tx.send(PoolEvent::InstanceExited { iid, code });
        });

        let now = Instant::now();
        let mut inst = Instance {
            pid,
            state: State::Ready,
            stdin: Some(in_tx),
            stop: Some(stop_tx),
            in_flight: 0,
            served: 0,
            started: now,
            idle_since: now,
            init_id: None,
        };
        if self.cfg.mode == Mode::Legacy {
            let bid = self.next_id;
            self.next_id += 1;
            inst.state = State::Starting;
            inst.init_id = Some(bid);
            let params = self.cfg.init_params.clone().unwrap_or_else(|| json!({}));
            let _ = inst.stdin.as_ref().unwrap().send(json!({"jsonrpc":"2.0","id":bid,"method":"initialize","params":params}).to_string());
        }
        tracing::info!(pool = %self.cfg.name, pid, instances = self.instances.len() + 1, "instance started");
        self.instances.insert(iid, inst);
    }

    /// Close stdin and let the supervisor escalate to signals if needed.
    fn stop_instance(&mut self, iid: InstanceId) {
        if let Some(inst) = self.instances.get_mut(&iid) {
            inst.state = State::Draining;
            inst.stdin = None;
            if let Some(stop) = inst.stop.take() {
                let _ = stop.send(());
            }
        }
    }

    fn instance_exited(&mut self, iid: InstanceId, code: Option<i32>) {
        let Some(inst) = self.instances.remove(&iid) else { return };
        self.daemon.shared_instances.fetch_sub(1, Ordering::Relaxed);
        let unexpected = inst.stop.is_some();
        if unexpected {
            tracing::warn!(pool = %self.cfg.name, pid = inst.pid, ?code, "instance exited unexpectedly");
        }
        let lost: Vec<i64> = self.pending.iter().filter(|(_, p)| p.iid == iid).map(|(b, _)| *b).collect();
        for bid in lost {
            if let Some(p) = self.pending.remove(&bid) {
                self.by_session.remove(&(p.sid, id_key(&p.orig_id)));
                if let Some(tok) = &p.progress {
                    self.progress.remove(tok);
                }
                let err = jsonrpc::error(&p.orig_id, jsonrpc::INTERNAL_ERROR, "MCP server process exited while handling this request");
                self.send(p.sid, &err);
            }
        }
        for (_, holder) in self.subscriptions.values_mut() {
            if *holder == Some(iid) {
                *holder = None;
            }
        }
        if inst.state == State::Starting {
            self.note_failure(format!("server exited during startup (code {code:?}); see {}", super::server_log(&self.cfg.name).display()));
        }
    }

    fn note_failure(&mut self, why: String) {
        self.failures += 1;
        let backoff = Duration::from_millis(250 * 2u64.pow(self.failures.min(6)));
        self.retry_at = Some(Instant::now() + backoff);
        let alive = self.instances.values().any(|i| i.state != State::Draining);
        if self.failures >= MAX_FAILURES && !alive {
            tracing::error!(pool = %self.cfg.name, "giving up after {} failures: {why}", self.failures);
            for (sid, id) in std::mem::take(&mut self.waiting_init) {
                self.send(sid, &jsonrpc::error(&id, jsonrpc::INTERNAL_ERROR, why.clone()));
            }
            for q in std::mem::take(&mut self.queue) {
                self.send(q.sid, &jsonrpc::error(&q.msg["id"], jsonrpc::INTERNAL_ERROR, why.clone()));
            }
            for s in self.sessions.values() {
                let _ = s.tx.send(wire::stderr_frame(&format!("[{}] {why}", brand::NAME)));
            }
        }
    }

    // ---------------------------------------------------------- scaling

    fn autoscale(&mut self) {
        let s = self.cfg.manifest.scaling.clone();
        if self.retry_at.is_some_and(|t| Instant::now() < t) {
            return;
        }
        if self.failures >= MAX_FAILURES && self.waiting_init.is_empty() && self.queue.is_empty() {
            return;
        }
        let active = self.instances.values().filter(|i| i.state != State::Draining).count() as u32;
        let starting = self.instances.values().filter(|i| i.state == State::Starting).count() as u32;
        let wanted = !self.sessions.is_empty() || !self.queue.is_empty() || !self.waiting_init.is_empty();
        let floor = if wanted { s.min_instances.max(1) } else { 0 };
        let budget = self.daemon.config.max_shared_instances;
        let daemon = self.daemon.clone();
        let can_spawn = |active: u32| active < s.max_instances && daemon.shared_instances.load(Ordering::Relaxed) < budget;

        let mut active = active;
        let mut starting = starting;
        while active < floor && can_spawn(active) {
            self.spawn_instance();
            active += 1;
            starting += 1;
        }
        // Capacity wanted beyond what is running: enough instances (at target
        // load) to absorb the queue, or one more if every instance is above target.
        let target = s.target() as usize;
        let ready: Vec<&Instance> = self.instances.values().filter(|i| i.state == State::Ready).collect();
        let over_target = !ready.is_empty() && ready.iter().all(|i| i.in_flight as usize > target);
        let for_queue = self.queue.len().div_ceil(target) as u32;
        let extra = for_queue.max(over_target as u32);
        if extra > starting {
            tracing::info!(pool = %self.cfg.name, active, queued = self.queue.len(), "scaling up");
        }
        while starting < extra && can_spawn(active) {
            self.spawn_instance();
            active += 1;
            starting += 1;
        }
    }

    fn tick(&mut self) {
        let s = self.cfg.manifest.scaling.clone();
        let now = Instant::now();
        let rss = self.daemon.rss.lock().unwrap().clone();

        // Startup timeouts, recycling, and finished drains.
        let mut to_stop = vec![];
        for (iid, inst) in self.instances.iter_mut() {
            match inst.state {
                State::Starting if now.duration_since(inst.started).as_secs() >= s.startup_timeout_seconds => {
                    tracing::warn!(pool = %self.cfg.name, pid = inst.pid, "startup timed out");
                    to_stop.push(*iid);
                }
                State::Ready => {
                    let mb = rss.get(&inst.pid).copied().unwrap_or(0) / 1024;
                    if s.max_memory_mb.is_some_and(|max| mb > max) {
                        tracing::info!(pool = %self.cfg.name, pid = inst.pid, mb, "recycling: memory limit");
                        inst.state = State::Draining;
                    }
                }
                _ => {}
            }
            if inst.state == State::Draining && inst.in_flight == 0 && inst.stop.is_some() {
                to_stop.push(*iid);
            }
        }
        for iid in to_stop {
            let starting = self.instances.get(&iid).is_some_and(|i| i.state == State::Starting);
            self.stop_instance(iid);
            if starting {
                self.note_failure("server did not finish initialize in time".into());
            }
        }

        // Scale down: newest idle instance above the floor, one per tick.
        let wanted = !self.sessions.is_empty() || !self.queue.is_empty();
        let floor = if wanted { s.min_instances.max(1) } else { 0 };
        // A stateless pool for a server that turned out to be pre-2026 will never be reused.
        let dead_end = self.cfg.mode == Mode::Stateless && self.daemon.legacy_only.lock().unwrap().contains(&self.cfg.base_key);
        let linger = match (self.sessions.is_empty(), dead_end) {
            (true, true) => 0,
            (true, false) => s.idle_timeout_seconds,
            (false, _) => s.scale_down_after_seconds,
        };
        let active = self.instances.values().filter(|i| i.state != State::Draining).count() as u32;
        if active > floor {
            let held: HashSet<InstanceId> = self.subscriptions.values().filter_map(|(_, h)| *h).collect();
            let victim = self
                .instances
                .iter()
                .rev()
                .find(|(iid, i)| {
                    i.state == State::Ready
                        && i.in_flight == 0
                        && now.duration_since(i.idle_since).as_secs() >= linger
                        && !held.contains(iid)
                })
                .map(|(iid, _)| *iid);
            if let Some(iid) = victim {
                tracing::info!(pool = %self.cfg.name, active, "scaling down");
                self.stop_instance(iid);
            }
        }
    }

    fn snapshot(&self) -> PoolSnapshot {
        let rss = self.daemon.rss.lock().unwrap();
        let s = &self.cfg.manifest.scaling;
        PoolSnapshot {
            name: self.cfg.name.clone(),
            identity: self.cfg.identity.id(),
            key: self.cfg.key.clone(),
            mode: self.cfg.mode,
            manifest_source: self.cfg.source.clone(),
            reason: self.cfg.manifest.reason.clone(),
            sessions: self.sessions.len(),
            queued: self.queue.len(),
            max_instances: s.max_instances,
            max_concurrent_requests: s.max_concurrent_requests,
            instances: self
                .instances
                .values()
                .map(|i| InstanceSnapshot {
                    pid: i.pid,
                    state: match i.state {
                        State::Starting => "starting",
                        State::Ready => "ready",
                        State::Draining => "draining",
                    },
                    in_flight: i.in_flight,
                    served: i.served,
                    rss_kb: rss.get(&i.pid).copied().unwrap_or(0),
                    age_secs: i.started.elapsed().as_secs(),
                })
                .collect(),
        }
    }
}
