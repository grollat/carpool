//! `isolation: session`: one private server process per client session, bytes
//! relayed untouched. This is exactly what the client would have done without
//! Carpool, plus lifecycle tracking, process-group cleanup and in-band manifest
//! discovery.

use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, oneshot};

use super::process::{self, LaunchSpec};
use super::{Daemon, DedicatedInfo};
use crate::identity::Identity;
use crate::{brand, wire};

#[allow(clippy::too_many_arguments)]
pub async fn run(
    daemon: Arc<Daemon>,
    sid: u64,
    name: String,
    identity: Identity,
    source: String,
    spec: LaunchSpec,
    pending: Vec<String>,
    mut client: Lines<BufReader<OwnedReadHalf>>,
    mut out: OwnedWriteHalf,
) {
    let mut child = match process::spawn(&spec) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("[{}] failed to start {}: {e}", brand::NAME, spec.program.display());
            let _ = out.write_all(wire::stderr_frame(&msg).as_bytes()).await;
            let _ = out.write_all(wire::exit_frame(127).as_bytes()).await;
            return;
        }
    };
    let pid = child.id().map(|p| p as i32).unwrap_or(0);
    daemon.dedicated.lock().unwrap().insert(
        sid,
        DedicatedInfo { name: name.clone(), identity: identity.id(), pid, source, started: Instant::now() },
    );
    tracing::info!(server = %name, pid, "dedicated instance started");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();

    // Single writer to the shim socket.
    let writer = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if out.write_all(frame.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    // Server stdout -> client, sniffing the handshake result for an in-band manifest.
    let tx = out_tx.clone();
    let d = daemon.clone();
    let ident = identity.clone();
    let reader = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        let mut sniffing = true;
        while let Ok(Some(line)) = lines.next_line().await {
            // The first initialize / server/discover result says whether the server
            // declares a manifest in-band.
            if sniffing && (line.contains("\"serverInfo\"") || line.contains("\"supportedVersions\"")) {
                sniffing = false;
                if let Some(result) = serde_json::from_str::<Value>(&line).ok().and_then(|v| v.get("result").cloned()) {
                    d.discovery.observe(&ident, &result);
                }
            }
            if tx.send(wire::msg_frame(&line)).is_err() {
                break;
            }
        }
    });

    // Server stderr -> client stderr (as the client would have seen it) and the log.
    let tx = out_tx.clone();
    let log_name = name.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        let mut log = super::open_server_log(&log_name).await;
        while let Ok(Some(line)) = lines.next_line().await {
            let _ = tx.send(wire::stderr_frame(&line));
            if let Some(f) = log.as_mut() {
                let _ = f.write_all(format!("[{pid}] {line}\n").as_bytes()).await;
            }
        }
    });

    let mut supervisor = tokio::spawn(process::supervise(child, stop_rx));

    // Client -> server stdin. Owning stdin means it closes when the client does.
    let mut pump = tokio::spawn(async move {
        for line in pending {
            if stdin.write_all(line.as_bytes()).await.is_err() || stdin.write_all(b"\n").await.is_err() {
                return;
            }
        }
        let _ = stdin.flush().await;
        while let Ok(Some(line)) = client.next_line().await {
            if stdin.write_all(line.as_bytes()).await.is_err() || stdin.write_all(b"\n").await.is_err() {
                break;
            }
            let _ = stdin.flush().await;
        }
    });

    let server_exited = tokio::select! {
        _ = &mut pump => None,
        res = &mut supervisor => Some(res.ok().flatten()),
    };
    let code = match server_exited {
        Some(code) => {
            pump.abort();
            let _ = reader.await;
            code
        }
        None => {
            // Client went away and stdin is closed; escalate if the server lingers.
            let _ = stop_tx.send(());
            supervisor.await.ok().flatten()
        }
    };
    let _ = out_tx.send(wire::exit_frame(code.unwrap_or(0)));
    drop(out_tx);
    let _ = writer.await;
    daemon.dedicated.lock().unwrap().remove(&sid);
    tracing::info!(server = %name, pid, ?code, "dedicated instance stopped");
}
