use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use crate::brand;

/// Root of all runtime state: `$CARPOOL_HOME`, default `~/.carpool`.
pub fn home() -> PathBuf {
    if let Some(p) = std::env::var_os(brand::HOME_ENV) {
        return PathBuf::from(p);
    }
    user_home().join(format!(".{}", brand::NAME))
}

pub fn user_home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"))
}

pub fn socket() -> PathBuf {
    if let Some(p) = std::env::var_os(brand::SOCKET_ENV) {
        return PathBuf::from(p);
    }
    home().join("run").join("daemon.sock")
}

pub fn lock_file() -> PathBuf {
    home().join("run").join("daemon.lock")
}

pub fn logs() -> PathBuf {
    home().join("logs")
}

pub fn daemon_log() -> PathBuf {
    logs().join("daemon.log")
}

/// User-authored manifests that override anything a server declares.
pub fn overrides() -> PathBuf {
    home().join("manifests")
}

pub fn cache() -> PathBuf {
    home().join("cache")
}

pub fn config_file() -> PathBuf {
    home().join("config.toml")
}

/// Create the state directories with owner-only permissions.
pub fn ensure() -> std::io::Result<()> {
    for dir in [home(), home().join("run"), logs(), overrides(), cache().join("learned"), cache().join("remote")] {
        std::fs::create_dir_all(&dir)?;
    }
    for dir in [home(), home().join("run")] {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    if let Some(parent) = socket().parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

/// Turn an arbitrary id like `npm:@scope/pkg` into a safe file name.
pub fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_' { c } else { '_' })
        .collect()
}
