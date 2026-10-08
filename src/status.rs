use std::time::Duration;

use anyhow::Result;
use serde_json::Value;

use crate::daemon::{self, Snapshot};
use crate::wire::Hello;

pub async fn fetch() -> Result<Snapshot> {
    Ok(serde_json::from_value(daemon::request(&Hello::Status).await?)?)
}

pub async fn run(json: bool, watch: bool) -> Result<()> {
    loop {
        let snap = match fetch().await {
            Ok(s) => s,
            Err(_) if !watch => {
                println!("daemon is not running (it starts automatically with the first session)");
                return Ok(());
            }
            Err(_) => {
                println!("\x1b[2J\x1b[Hdaemon is not running");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        if json {
            println!("{}", serde_json::to_string_pretty(&snap)?);
        } else {
            if watch {
                print!("\x1b[2J\x1b[H");
            }
            print!("{}", render(&snap));
        }
        if !watch {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn mb(kb: u64) -> String {
    format!("{:.0} MB", kb as f64 / 1024.0)
}

pub fn render(s: &Snapshot) -> String {
    let t = &s.totals;
    let mut out = String::new();
    out.push_str(&format!(
        "carpool {} · daemon pid {} · up {}s\n\n",
        s.version, s.pid, s.uptime_secs
    ));
    out.push_str(&format!(
        "  {} sessions  →  {} server processes  ({} RSS)\n",
        t.sessions, t.server_processes, mb(t.rss_mb * 1024)
    ));
    if t.processes_without_carpool > t.server_processes {
        out.push_str(&format!(
            "  without carpool: {} processes  ·  saved ≈ {} processes, ≈ {}\n",
            t.processes_without_carpool,
            t.processes_without_carpool - t.server_processes,
            mb(t.est_rss_saved_mb * 1024)
        ));
    }
    if !s.pools.is_empty() {
        out.push_str("\nSHARED POOLS\n");
        for p in &s.pools {
            let instances = p["instances"].as_array().cloned().unwrap_or_default();
            out.push_str(&format!(
                "  {}  [{}]  {} sessions · {}/{} instances · {} queued · {}\n",
                p["name"].as_str().unwrap_or("?"),
                p["mode"].as_str().unwrap_or("?"),
                p["sessions"],
                instances.len(),
                p["max_instances"],
                p["queued"],
                p["manifest_source"].as_str().unwrap_or(""),
            ));
            for i in &instances {
                out.push_str(&format!(
                    "    pid {:<7} {:<9} in-flight {:>2}/{:<3} served {:<6} {:>7}  {}s\n",
                    i["pid"],
                    i["state"].as_str().unwrap_or(""),
                    i["in_flight"],
                    p["max_concurrent_requests"],
                    i["served"],
                    mb(i["rss_kb"].as_u64().unwrap_or(0)),
                    i["age_secs"],
                ));
            }
        }
    }
    if !s.dedicated.is_empty() {
        out.push_str("\nPER-SESSION (isolated)\n");
        let mut groups: std::collections::BTreeMap<(String, String), Vec<&daemon::DedicatedSnapshot>> = Default::default();
        for d in &s.dedicated {
            groups.entry((d.name.clone(), d.manifest_source.clone())).or_default().push(d);
        }
        for ((name, source), ds) in groups {
            let kb: u64 = ds.iter().map(|d| d.rss_kb).sum();
            out.push_str(&format!("  {name}  {} processes · {} · {source}\n", ds.len(), mb(kb)));
        }
    }
    out
}

#[allow(dead_code)]
pub fn pool_instances(s: &Snapshot, name: &str) -> Vec<Value> {
    s.pools.iter().filter(|p| p["name"] == name).flat_map(|p| p["instances"].as_array().cloned().unwrap_or_default()).collect()
}
