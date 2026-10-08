//! `carpool run -- <cmd> <args>`: the process an MCP client launches instead of
//! the server. Deliberately tiny (plain threads, no async runtime): it connects
//! to the daemon, starting it if needed, and relays bytes. If the daemon can't
//! be reached it execs the server directly so the client never breaks.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::manifest::Manifest;
use crate::wire::{Hello, SessionHello, PROTO};
use crate::{brand, paths};

pub struct RunArgs {
    pub name: Option<String>,
    pub manifest: Option<PathBuf>,
    pub isolate: bool,
    pub command: Vec<String>,
}

pub fn run(args: RunArgs) -> Result<()> {
    let (command, server_args) = args.command.split_first().context("missing server command after `--`")?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));

    let manifest = match &args.manifest {
        Some(p) => {
            let text = std::fs::read_to_string(p).with_context(|| format!("reading manifest {}", p.display()))?;
            let value: serde_json::Value = serde_json::from_str(&text).context("manifest is not JSON")?;
            Manifest::from_value(&value).with_context(|| format!("invalid manifest {}", p.display()))?;
            Some(value)
        }
        None => None,
    };

    let hello = Hello::Session(SessionHello {
        proto: PROTO,
        name: args.name.clone().unwrap_or_else(|| default_name(command, server_args)),
        command: command.clone(),
        args: server_args.to_vec(),
        env: std::env::vars().collect(),
        cwd: cwd.display().to_string(),
        manifest,
        force_session: args.isolate,
        pid: std::process::id(),
    });

    let stream = match connect_or_start() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[{}] daemon unavailable ({e:#}); running {command} directly", brand::NAME);
            let err = Command::new(command).args(server_args).exec();
            anyhow::bail!("failed to exec {command}: {err}");
        }
    };

    let mut writer = stream.try_clone()?;
    writer.write_all(serde_json::to_string(&hello)?.as_bytes())?;
    writer.write_all(b"\n")?;

    // Client -> daemon: raw bytes; MCP stdio is already newline-delimited.
    std::thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if writer.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = writer.shutdown(std::net::Shutdown::Write);
    });

    // Daemon -> client: demultiplex tagged lines.
    let mut reader = BufReader::with_capacity(64 * 1024, stream);
    let mut line = Vec::with_capacity(64 * 1024);
    let stdout = io::stdout();
    let stderr = io::stderr();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            std::process::exit(0);
        }
        let Some((&tag, rest)) = line.split_first() else { continue };
        match tag {
            b'M' => {
                let mut out = stdout.lock();
                out.write_all(rest)?;
                out.flush()?;
            }
            b'E' => {
                let mut err = stderr.lock();
                let _ = err.write_all(rest);
            }
            b'X' => {
                let code = std::str::from_utf8(rest).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(1);
                std::process::exit(code);
            }
            _ => {}
        }
    }
}

fn default_name(command: &str, args: &[String]) -> String {
    let id = crate::identity::Identity::from_command(command, args, &std::env::current_dir().unwrap_or_default());
    id.name.rsplit('/').next().unwrap_or(&id.name).to_string()
}

/// Connect to the daemon, starting it in the background on first use. A start
/// can lose a race with a daemon that is shutting down, so keep retrying.
pub fn connect_or_start() -> Result<UnixStream> {
    let sock = paths::socket();
    if let Ok(s) = UnixStream::connect(&sock) {
        return Ok(s);
    }
    let start = Instant::now();
    let deadline = start + Duration::from_secs(10);
    let mut last_spawn: Option<Instant> = None;
    let mut wait = Duration::from_millis(10);
    loop {
        if last_spawn.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
            spawn_daemon()?;
            last_spawn = Some(Instant::now());
        }
        match UnixStream::connect(&sock) {
            Ok(s) => return Ok(s),
            Err(e) if Instant::now() >= deadline => {
                return Err(e).with_context(|| format!("connecting to {}", sock.display()))
            }
            Err(_) => {
                std::thread::sleep(wait);
                wait = (wait * 2).min(Duration::from_millis(100));
            }
        }
    }
}

pub fn spawn_daemon() -> Result<()> {
    paths::ensure()?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(paths::daemon_log())?;
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("daemon").stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    // Detach into its own session so it outlives the client that started it.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("starting daemon")?;
    Ok(())
}
