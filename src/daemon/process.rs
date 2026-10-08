//! Spawning and terminating MCP server processes. Each server runs in its own
//! process group so wrappers like `npx` or `uvx` are cleaned up with their child.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command};
use tokio::sync::oneshot;

/// Everything needed to start one server process.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LaunchSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
}

/// Variables every server gets so that interpreters, package runners and TLS work.
pub const BASELINE_ENV: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "LC_ALL", "LC_CTYPE", "TMPDIR", "TZ", "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME", "XDG_DATA_HOME", "HTTP_PROXY", "HTTPS_PROXY", "NO_PROXY", "http_proxy", "https_proxy", "no_proxy",
    "SSL_CERT_FILE", "SSL_CERT_DIR", "NODE_EXTRA_CA_CERTS", "REQUESTS_CA_BUNDLE", "DOCKER_HOST", "DOCKER_CONTEXT",
    "DOCKER_CONFIG", "npm_config_registry", "NPM_CONFIG_REGISTRY", "UV_INDEX_URL", "UV_DEFAULT_INDEX", "PIP_INDEX_URL",
];

/// Find `command` the way a shell would, using the *session's* PATH.
pub fn resolve_program(command: &str, env: &BTreeMap<String, String>, cwd: &Path) -> PathBuf {
    if command.contains('/') {
        let p = Path::new(command);
        return if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
    }
    if let Some(path) = env.get("PATH") {
        for dir in path.split(':').filter(|d| !d.is_empty()) {
            let candidate = Path::new(dir).join(command);
            if is_executable(&candidate) {
                return candidate;
            }
        }
    }
    PathBuf::from(command)
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

pub fn spawn(spec: &LaunchSpec) -> std::io::Result<Child> {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .env_clear()
        .envs(&spec.env)
        .current_dir(&spec.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let child = cmd.spawn()?;
    if let Some(pid) = child.id() {
        crate::daemon::track_group(pid as i32);
    }
    Ok(child)
}

pub fn killpg(pgid: i32, signal: i32) {
    if pgid > 0 {
        unsafe {
            libc::killpg(pgid, signal);
        }
    }
}

/// Wait for `child` to exit. If `stop` fires first (or its sender is dropped),
/// give the server a grace period after stdin was closed, then SIGTERM, then
/// SIGKILL the whole process group. Returns the exit code.
pub async fn supervise(mut child: Child, stop: oneshot::Receiver<()>) -> Option<i32> {
    let pid = child.id().map(|p| p as i32).unwrap_or(0);
    let early = tokio::select! {
        status = child.wait() => Some(status),
        _ = stop => None,
    };
    let status = match early {
        Some(s) => s,
        None => match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
            Ok(s) => s,
            Err(_) => {
                killpg(pid, libc::SIGTERM);
                match tokio::time::timeout(Duration::from_secs(3), child.wait()).await {
                    Ok(s) => s,
                    Err(_) => {
                        killpg(pid, libc::SIGKILL);
                        child.wait().await
                    }
                }
            }
        },
    };
    // Wrapper exited; make sure nothing it spawned lingers in the group.
    killpg(pid, libc::SIGTERM);
    crate::daemon::untrack_group(pid);
    status.ok().and_then(|s| s.code())
}

/// Resident memory per process group, in KiB, from a single `ps` call.
pub async fn sample_rss() -> HashMap<i32, u64> {
    let mut out = HashMap::new();
    let Ok(output) = Command::new("ps").args(["-A", "-o", "pgid=,rss="]).output().await else {
        return out;
    };
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut it = line.split_whitespace();
        if let (Some(Ok(pgid)), Some(Ok(rss))) = (it.next().map(str::parse::<i32>), it.next().map(str::parse::<u64>)) {
            *out.entry(pgid).or_insert(0) += rss;
        }
    }
    out
}
