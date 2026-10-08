//! End-to-end tests: a real daemon, real `carpool run` shims, and the built-in
//! mock MCP server, exercised the way Claude Code / Codex would.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_carpool");
static N: AtomicU32 = AtomicU32::new(0);

struct Env {
    home: PathBuf,
    daemon: Child,
}

impl Env {
    fn new() -> Env {
        // Short path: Unix socket paths are limited to ~104 bytes on macOS.
        let home = PathBuf::from(format!("/tmp/carpool-t{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("config.toml"), "remote_discovery = false\n").unwrap();
        let log = std::fs::File::create(home.join("daemon.log")).unwrap();
        let daemon = Command::new(BIN)
            .arg("daemon")
            .env("CARPOOL_HOME", &home)
            .env("CARPOOL_LOG", "debug")
            .stderr(log)
            .spawn()
            .unwrap();
        let sock = home.join("run").join("daemon.sock");
        let t = Instant::now();
        while !sock.exists() {
            assert!(t.elapsed() < Duration::from_secs(10), "daemon did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        Env { home, daemon }
    }

    fn manifest(&self, name: &str, v: Value) -> String {
        let p = self.home.join(format!("{name}.json"));
        std::fs::write(&p, v.to_string()).unwrap();
        p.display().to_string()
    }

    fn session(&self, run_args: &[&str], mock_args: &[&str], env: &[(&str, &str)]) -> Session {
        let mut cmd = Command::new(BIN);
        cmd.arg("run").args(["--name", "mock"]).args(run_args).arg("--").arg(BIN).arg("mock-server").args(mock_args);
        cmd.env("CARPOOL_HOME", &self.home).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    if tx.send(v).is_err() {
                        break;
                    }
                }
            }
        });
        Session { child, stdin: Some(stdin), rx, backlog: vec![] }
    }

    fn status(&self) -> Value {
        let out = Command::new(BIN).args(["status", "--json"]).env("CARPOOL_HOME", &self.home).output().unwrap();
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    }

    fn instances(&self, pool: &str) -> Vec<Value> {
        let s = self.status();
        s["pools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|p| p["name"] == pool)
            .flat_map(|p| p["instances"].as_array().cloned().unwrap_or_default())
            .filter(|i| i["state"] != "draining")
            .collect()
    }

    fn wait_for(&self, what: &str, mut cond: impl FnMut() -> bool, secs: u64) {
        let t = Instant::now();
        while !cond() {
            assert!(t.elapsed() < Duration::from_secs(secs), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = Command::new(BIN).arg("stop").env("CARPOOL_HOME", &self.home).output();
        let _ = self.daemon.wait();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<Value>,
    backlog: Vec<Value>,
}

impl Session {
    fn send(&mut self, v: Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{v}").unwrap();
        stdin.flush().unwrap();
    }

    fn next(&mut self, pred: impl Fn(&Value) -> bool) -> Value {
        if let Some(i) = self.backlog.iter().position(&pred) {
            return self.backlog.remove(i);
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let v = self.rx.recv_timeout(left).expect("timed out waiting for a message");
            if pred(&v) {
                return v;
            }
            self.backlog.push(v);
        }
    }

    fn response(&mut self, id: Value) -> Value {
        self.next(move |v| v["id"] == id && v.get("method").is_none())
    }

    fn init(mut self) -> Self {
        self.send(json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{"roots":{}},"clientInfo":{"name":"e2e","version":"1"}}}));
        let r = self.response(json!(0));
        assert!(r.get("result").is_some(), "initialize failed: {r}");
        self.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        self
    }

    fn start_call(&mut self, id: i64, tool: &str, args: Value) {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":tool,"arguments":args}}));
    }

    fn call(&mut self, id: i64, tool: &str, args: Value) -> Value {
        self.start_call(id, tool, args);
        self.response(json!(id))
    }

    fn text(&mut self, id: i64, tool: &str, args: Value) -> String {
        let r = self.call(id, tool, args);
        r.pointer("/result/content/0/text").and_then(Value::as_str).unwrap_or_else(|| panic!("no text in {r}")).to_string()
    }

    fn whoami(&mut self, id: i64) -> Value {
        serde_json::from_str(&self.text(id, "whoami", json!({}))).unwrap()
    }

    fn close(mut self) -> std::process::ExitStatus {
        drop(self.stdin.take());
        self.child.wait().unwrap()
    }
}

fn shared(extra: Value) -> Value {
    let mut m = json!({"isolation": "shared", "partition": {"cwd": false}});
    if let Some(o) = extra.as_object() {
        for (k, v) in o {
            m[k] = v.clone();
        }
    }
    m
}

fn pid_alive(pid: i64) -> bool {
    unsafe { libc_kill(pid as i32, 0) == 0 }
}

extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

// ----------------------------------------------------------------- tests

#[test]
fn without_manifest_every_session_gets_its_own_process() {
    let env = Env::new();
    let mut a = env.session(&[], &[], &[]).init();
    let mut b = env.session(&[], &[], &[]).init();
    let pa = a.whoami(1)["pid"].as_i64().unwrap();
    let pb = b.whoami(1)["pid"].as_i64().unwrap();
    assert_ne!(pa, pb, "default must be session isolation");

    a.text(2, "remember", json!({"value": "secret-of-a"}));
    assert_eq!(b.text(2, "recall", json!({})), "", "state leaked between isolated sessions");

    assert!(a.close().success());
    env.wait_for("dedicated process to stop", || !pid_alive(pa), 10);
    assert!(pid_alive(pb));
}

#[test]
fn shared_sessions_are_multiplexed_onto_one_process() {
    let env = Env::new();
    let m = env.manifest("m", shared(json!({})));
    let mut a = env.session(&["--manifest", &m], &[], &[]).init();
    let mut b = env.session(&["--manifest", &m], &[], &[]).init();

    // Same JSON-RPC id from both sessions, in flight at the same time.
    a.start_call(7, "sleep", json!({"ms": 300}));
    b.start_call(7, "echo", json!({"text": "from b"}));
    let rb = b.response(json!(7));
    let ra = a.response(json!(7));
    assert_eq!(rb.pointer("/result/content/0/text").unwrap(), "from b");
    assert_eq!(ra.pointer("/result/content/0/text").unwrap(), "slept 300ms");

    let pa = a.whoami(8)["pid"].as_i64().unwrap();
    let pb = b.whoami(8)["pid"].as_i64().unwrap();
    assert_eq!(pa, pb, "shared sessions should use one instance");
    let s = env.status();
    assert_eq!(s["totals"]["sessions"], 2);
    assert_eq!(s["totals"]["server_processes"], 1);
}

#[test]
fn pool_scales_up_under_load_and_back_down_when_idle() {
    let env = Env::new();
    let m = env.manifest(
        "m",
        shared(json!({"scaling": {"maxConcurrentRequests": 2, "targetConcurrentRequests": 1, "maxInstances": 3, "scaleDownAfterSeconds": 1}})),
    );
    let mut sessions: Vec<Session> = (0..6).map(|_| env.session(&["--manifest", &m], &[], &[]).init()).collect();
    for (i, s) in sessions.iter_mut().enumerate() {
        s.start_call(100 + i as i64, "sleep", json!({"ms": 1500}));
    }
    env.wait_for("scale up to 3 instances", || env.instances("mock").len() == 3, 10);
    for (i, s) in sessions.iter_mut().enumerate() {
        let r = s.response(json!(100 + i as i64));
        assert!(r.get("result").is_some(), "{r}");
    }
    // No instance ever exceeded maxConcurrentRequests.
    let mut pids = std::collections::HashSet::new();
    for s in sessions.iter_mut() {
        let w = s.whoami(200);
        assert!(w["peakConcurrency"].as_u64().unwrap() <= 2, "{w}");
        pids.insert(w["pid"].as_i64().unwrap());
    }
    assert!(pids.len() <= 3);
    env.wait_for("scale down to 1 instance", || env.instances("mock").len() == 1, 15);
}

#[test]
fn crashed_instance_fails_its_requests_and_is_replaced() {
    let env = Env::new();
    let m = env.manifest("m", shared(json!({})));
    let mut a = env.session(&["--manifest", &m], &[], &[]).init();
    let before = a.whoami(1)["pid"].as_i64().unwrap();
    let r = a.call(2, "crash", json!({}));
    assert!(r.get("error").is_some(), "crash should surface as an error: {r}");
    let after = a.whoami(3)["pid"].as_i64().unwrap();
    assert_ne!(before, after);
}

#[test]
fn in_band_declaration_is_learned_after_first_run() {
    let env = Env::new();
    let first = env.session(&[], &["--declare", "shared"], &[]).init();
    // The first session predates any knowledge: it runs isolated.
    let s = env.status();
    assert_eq!(s["dedicated"].as_array().unwrap().len(), 1);
    drop(first);
    let mut b = env.session(&[], &["--declare", "shared"], &[]).init();
    let mut c = env.session(&[], &["--declare", "shared"], &[]).init();
    assert_eq!(b.whoami(1)["pid"], c.whoami(1)["pid"], "learned shared manifest should apply");
    let s = env.status();
    assert_eq!(s["pools"][0]["manifest_source"], "declared by server");
}

#[test]
fn declared_env_partitions_pools_and_undeclared_env_is_not_passed() {
    let env = Env::new();
    let m = env.manifest("m", shared(json!({"partition": {"env": ["MOCK_SECRET"], "cwd": false}})));
    let mut a = env.session(&["--manifest", &m], &[], &[("MOCK_SECRET", "alice")]).init();
    let mut b = env.session(&["--manifest", &m], &[], &[("MOCK_SECRET", "bob")]).init();
    let mut a2 = env.session(&["--manifest", &m], &[], &[("MOCK_SECRET", "alice")]).init();
    let (wa, wb, wa2) = (a.whoami(1), b.whoami(1), a2.whoami(1));
    assert_eq!(wa["secret"], "alice");
    assert_eq!(wb["secret"], "bob");
    assert_ne!(wa["pid"], wb["pid"], "different credentials must never share");
    assert_eq!(wa["pid"], wa2["pid"], "same credentials should share");

    // An undeclared variable never reaches a shared instance.
    let m2 = env.manifest("m2", shared(json!({})));
    let mut c = env.session(&["--manifest", &m2], &[], &[("MOCK_SECRET", "carol")]).init();
    assert_eq!(c.whoami(1)["secret"], Value::Null);
}

#[test]
fn cwd_is_part_of_the_partition_by_default() {
    let env = Env::new();
    let m = env.manifest("m", json!({"isolation": "shared"}));
    let d1 = env.home.join("p1");
    let d2 = env.home.join("p2");
    std::fs::create_dir_all(&d1).unwrap();
    std::fs::create_dir_all(&d2).unwrap();
    let sess = |dir: &PathBuf| {
        let mut cmd = env.session(&["--manifest", &m], &[], &[]);
        drop(cmd.stdin.take());
        let _ = cmd.child.wait();
        let mut c = Command::new(BIN);
        c.current_dir(dir).arg("run").args(["--manifest", &m, "--", BIN, "mock-server"]).env("CARPOOL_HOME", &env.home);
        c
    };
    let mut c1 = sess(&d1);
    let mut c2 = sess(&d2);
    let ask = |c: &mut Command| -> Value {
        let mut child = c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        writeln!(stdin, "{}", json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t"}}})).unwrap();
        writeln!(stdin, "{}", json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"whoami","arguments":{}}})).unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut out = Value::Null;
        for line in lines.by_ref() {
            let v: Value = serde_json::from_str(&line.unwrap()).unwrap();
            if v["id"] == 1 {
                out = serde_json::from_str(v.pointer("/result/content/0/text").unwrap().as_str().unwrap()).unwrap();
                break;
            }
        }
        drop(stdin);
        let _ = child.wait();
        out
    };
    let w1 = ask(&mut c1);
    let w2 = ask(&mut c2);
    assert!(w1["cwd"].as_str().unwrap().ends_with("/p1"), "{w1}");
    assert!(w2["cwd"].as_str().unwrap().ends_with("/p2"), "{w2}");
    assert_ne!(w1["pid"], w2["pid"]);
}

#[test]
fn cancellation_frees_capacity_and_progress_tokens_are_routed() {
    let env = Env::new();
    let m = env.manifest("m", shared(json!({"scaling": {"maxConcurrentRequests": 4, "maxInstances": 1}})));
    let mut a = env.session(&["--manifest", &m], &[], &[]).init();
    let mut b = env.session(&["--manifest", &m], &[], &[]).init();

    // Both sessions use the same progress token concurrently.
    for s in [&mut a, &mut b] {
        s.send(json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"sleep","arguments":{"ms":200},"_meta":{"progressToken":"tok"}}}));
    }
    for s in [&mut a, &mut b] {
        let p = s.next(|v| v["method"] == "notifications/progress");
        assert_eq!(p["params"]["progressToken"], "tok");
        assert!(s.response(json!(5)).get("result").is_some());
    }

    a.start_call(9, "sleep", json!({"ms": 5000}));
    env.wait_for("request in flight", || env.instances("mock").first().is_some_and(|i| i["in_flight"] == 1), 5);
    a.send(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":9,"reason":"user"}}));
    env.wait_for("capacity released", || env.instances("mock").first().is_some_and(|i| i["in_flight"] == 0), 5);
}

#[test]
fn stateless_protocol_and_fallback_to_initialize() {
    let env = Env::new();
    let m = env.manifest("m", shared(json!({})));
    let meta = json!({"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}});

    // A 2026 client talking to a 2026 server: no initialize at all.
    let mut a = env.session(&["--manifest", &m], &[], &[]);
    a.send(json!({"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta":meta}}));
    assert!(a.response(json!(1)).get("result").is_some());
    a.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"echo","arguments":{"text":"hi"},"_meta":meta}}));
    assert_eq!(a.response(json!(2)).pointer("/result/content/0/text").unwrap(), "hi");

    // A 2026 client talking to an older server: discover fails, client falls back.
    let mut b = env.session(&["--manifest", &m], &["--legacy-only"], &[]);
    b.send(json!({"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta":meta}}));
    assert!(b.response(json!(1)).get("error").is_some());
    let mut b = b.init();
    assert_eq!(b.text(2, "echo", json!({"text":"legacy"})), "legacy");

    // The next probing client is answered without starting another process.
    let mut c = env.session(&["--manifest", &m], &["--legacy-only"], &[]);
    c.send(json!({"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta":meta}}));
    assert!(c.response(json!(1)).get("error").is_some());
    let mut c = c.init();
    assert_eq!(b.whoami(3)["pid"], c.whoami(3)["pid"]);
}

#[test]
fn isolate_flag_overrides_a_shared_manifest() {
    let env = Env::new();
    let m = env.manifest("m", shared(json!({})));
    let mut a = env.session(&["--manifest", &m, "--isolate"], &[], &[]).init();
    let mut b = env.session(&["--manifest", &m, "--isolate"], &[], &[]).init();
    assert_ne!(a.whoami(1)["pid"], b.whoami(1)["pid"]);
}

#[test]
fn operator_override_beats_server_declaration() {
    let env = Env::new();
    let dir = env.home.join("manifests");
    std::fs::create_dir_all(&dir).unwrap();
    let id = format!("path:{}", std::fs::canonicalize(BIN).unwrap().display());
    std::fs::write(dir.join("force.json"), json!({"isolation":"session","match":[id]}).to_string()).unwrap();
    let m = env.manifest("m", shared(json!({})));
    let mut a = env.session(&["--manifest", &m], &[], &[]).init();
    let mut b = env.session(&["--manifest", &m], &[], &[]).init();
    assert_ne!(a.whoami(1)["pid"], b.whoami(1)["pid"]);
}
