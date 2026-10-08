//! `carpool mock-server`: a tiny MCP server for tests, benchmarks and demos.
//! It speaks both the initialize handshake and the stateless protocol, and its
//! tools expose what matters when verifying a runtime: which process answered,
//! how many calls it has served, and the peak concurrency it observed.

use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::brand;

pub struct MockArgs {
    pub declare: Option<String>,
    pub startup_ms: u64,
    pub legacy_only: bool,
}

#[derive(Default)]
struct Stats {
    calls: AtomicU64,
    in_flight: AtomicU64,
    peak: AtomicU64,
    memory: Mutex<Vec<String>>,
}

pub fn run(args: MockArgs) -> anyhow::Result<()> {
    std::thread::sleep(Duration::from_millis(args.startup_ms));
    let out = Arc::new(Mutex::new(std::io::stdout()));
    let stats = Arc::new(Stats::default());
    let send = |out: &Arc<Mutex<std::io::Stdout>>, v: Value| {
        let mut o = out.lock().unwrap();
        let _ = writeln!(o, "{v}");
        let _ = o.flush();
    };
    let capabilities = {
        let mut c = json!({"tools": {"listChanged": false}, "resources": {"subscribe": true}});
        if let Some(iso) = &args.declare {
            c["experimental"] = json!({ brand::INBAND_KEY: {"isolation": iso, "scaling": {"maxConcurrentRequests": 4, "maxInstances": 3}} });
        }
        c
    };

    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(id) = msg.get("id").cloned() else { continue };
        let Some(method) = msg.get("method").and_then(Value::as_str) else { continue };
        let ok = |r: Value| json!({"jsonrpc": "2.0", "id": id, "result": r});
        match method {
            "initialize" => {
                let proto = msg.pointer("/params/protocolVersion").cloned().unwrap_or(json!("2025-06-18"));
                send(&out, ok(json!({
                    "protocolVersion": proto,
                    "capabilities": capabilities,
                    "serverInfo": {"name": "mock", "version": brand::VERSION},
                })));
            }
            "server/discover" if !args.legacy_only => send(&out, ok(json!({
                "supportedVersions": ["2026-07-28"],
                "capabilities": capabilities,
                "serverInfo": {"name": "mock", "version": brand::VERSION},
            }))),
            "ping" => send(&out, ok(json!({}))),
            "tools/list" => send(&out, ok(json!({"tools": [
                {"name": "echo", "description": "Echo text back", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}},
                {"name": "sleep", "description": "Wait ms milliseconds", "inputSchema": {"type": "object", "properties": {"ms": {"type": "integer"}}}},
                {"name": "whoami", "description": "Report pid, calls served and peak concurrency of this server process", "inputSchema": {"type": "object"}},
                {"name": "remember", "description": "Store a value in process memory", "inputSchema": {"type": "object", "properties": {"value": {"type": "string"}}}},
                {"name": "recall", "description": "List values stored in process memory", "inputSchema": {"type": "object"}},
                {"name": "crash", "description": "Exit the process", "inputSchema": {"type": "object"}},
            ]}))),
            "resources/subscribe" | "resources/unsubscribe" => send(&out, ok(json!({}))),
            "tools/call" => {
                let out = out.clone();
                let stats = stats.clone();
                std::thread::spawn(move || {
                    let n = stats.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    stats.peak.fetch_max(n, Ordering::SeqCst);
                    let calls = stats.calls.fetch_add(1, Ordering::SeqCst) + 1;
                    let name = msg.pointer("/params/name").and_then(Value::as_str).unwrap_or("");
                    let a = msg.pointer("/params/arguments").cloned().unwrap_or(json!({}));
                    let text = match name {
                        "echo" => a["text"].as_str().unwrap_or("").to_string(),
                        "sleep" => {
                            let ms = a["ms"].as_u64().unwrap_or(100);
                            if let Some(tok) = msg.pointer("/params/_meta/progressToken") {
                                let p = json!({"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":tok,"progress":0.5,"total":1}});
                                let mut o = out.lock().unwrap();
                                let _ = writeln!(o, "{p}");
                                let _ = o.flush();
                            }
                            std::thread::sleep(Duration::from_millis(ms));
                            format!("slept {ms}ms")
                        }
                        "whoami" => json!({
                            "pid": std::process::id(),
                            "calls": calls,
                            "peakConcurrency": stats.peak.load(Ordering::SeqCst),
                            "cwd": std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default(),
                            "secret": std::env::var("MOCK_SECRET").ok(),
                        })
                        .to_string(),
                        "remember" => {
                            stats.memory.lock().unwrap().push(a["value"].as_str().unwrap_or("").to_string());
                            "stored".into()
                        }
                        "recall" => stats.memory.lock().unwrap().join(","),
                        "crash" => std::process::exit(3),
                        other => format!("unknown tool {other}"),
                    };
                    stats.in_flight.fetch_sub(1, Ordering::SeqCst);
                    let r = json!({"jsonrpc":"2.0","id":msg["id"],"result":{"content":[{"type":"text","text":text}]}});
                    let mut o = out.lock().unwrap();
                    let _ = writeln!(o, "{r}");
                    let _ = o.flush();
                });
            }
            _ => send(&out, json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":format!("Method not found: {method}")}})),
        }
    }
    Ok(())
}
