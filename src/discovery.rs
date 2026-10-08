//! Where a server's manifest comes from, in order of precedence:
//!
//! 1. `carpool run --isolate`                        (forced session)
//! 2. `~/.carpool/manifests/*.json` with `match`     (operator override)
//! 3. `carpool run --manifest <file>`                (operator, inline)
//! 4. `carpool.json` / package.json `"carpool"` key shipped in the installed package
//! 5. in-band `capabilities.experimental["carpool/manifest"]`, learned from a previous run
//! 6. the package's npm metadata or source repository (fetched in the background, cached)
//! 7. the community registry bundled with this binary
//! 8. default: `isolation: session` -- never shared.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::identity::{Identity, Kind};
use crate::manifest::Manifest;
use crate::wire::SessionHello;
use crate::{brand, paths};

mod bundled {
    include!(concat!(env!("OUT_DIR"), "/bundled.rs"));
}

#[derive(Debug, Clone)]
pub enum Source {
    Forced,
    Override(PathBuf),
    Inline,
    Package(PathBuf),
    InBand,
    Remote(String),
    Bundled(String),
    Default,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::Forced => write!(f, "forced by --isolate"),
            Source::Override(p) => write!(f, "override {}", p.display()),
            Source::Inline => write!(f, "--manifest"),
            Source::Package(p) => write!(f, "package {}", p.display()),
            Source::InBand => write!(f, "declared by server"),
            Source::Remote(u) => write!(f, "published {u}"),
            Source::Bundled(n) => write!(f, "community registry {n}"),
            Source::Default => write!(f, "default (no manifest)"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub manifest: Manifest,
    pub source: Source,
}

#[derive(Serialize, Deserialize)]
struct RemoteEntry {
    fetched_at: u64,
    source: String,
    manifest: Option<Value>,
}

const REMOTE_TTL: u64 = 24 * 3600;
const REMOTE_NEGATIVE_TTL: u64 = 6 * 3600;

pub struct Discovery {
    remote: bool,
    learned: Mutex<HashMap<String, Manifest>>,
    fetching: Arc<Mutex<HashSet<String>>>,
    bundled: Vec<(String, Manifest)>,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn bundled_manifests() -> Vec<(String, Manifest)> {
    bundled::BUNDLED
        .iter()
        .filter_map(|(file, text)| match Manifest::parse(text) {
            Ok(m) => Some((file.to_string(), m)),
            Err(e) => {
                tracing::error!("bundled manifest {file} is invalid: {e:#}");
                None
            }
        })
        .collect()
}

impl Discovery {
    pub fn new(remote: bool) -> Discovery {
        Discovery { remote, learned: Mutex::new(HashMap::new()), fetching: Default::default(), bundled: bundled_manifests() }
    }

    pub fn resolve(&self, h: &SessionHello, ident: &Identity) -> Resolved {
        let found = |manifest: Manifest, source: Source| Resolved { manifest, source };
        if h.force_session {
            return found(Manifest::session_default(), Source::Forced);
        }
        if let Some((m, p)) = find_override(ident) {
            return found(m, Source::Override(p));
        }
        if let Some(v) = &h.manifest {
            match Manifest::from_value(v) {
                Ok(m) => return found(m, Source::Inline),
                Err(e) => tracing::warn!("ignoring invalid --manifest: {e:#}"),
            }
        }
        let home = h.env.get("HOME").map(PathBuf::from).unwrap_or_else(paths::user_home);
        let program = crate::daemon::process::resolve_program(&h.command, &h.env, Path::new(&h.cwd));
        if let Some((m, p)) = find_in_package(ident, &home, &program) {
            return found(m, Source::Package(p));
        }
        if let Some(m) = self.learned(ident) {
            return found(m, Source::InBand);
        }
        if let Some((m, src)) = self.remote(ident) {
            return found(m, Source::Remote(src));
        }
        if let Some((file, m)) = self.bundled.iter().find(|(_, m)| m.matches.iter().any(|p| ident.matches(p))) {
            if m.applies_to_version(ident.version.as_deref()) {
                return found(m.clone(), Source::Bundled(file.clone()));
            }
        }
        found(Manifest::session_default(), Source::Default)
    }

    // ------------------------------------------------------------- in-band

    fn learned(&self, ident: &Identity) -> Option<Manifest> {
        if let Some(m) = self.learned.lock().unwrap().get(&ident.id()) {
            return Some(m.clone());
        }
        let text = std::fs::read_to_string(learned_path(ident)).ok()?;
        let m = Manifest::parse(&text).ok()?;
        self.learned.lock().unwrap().insert(ident.id(), m.clone());
        Some(m)
    }

    /// Record what a server said about itself in its initialize / discover result.
    /// A server that stops declaring a manifest is forgotten.
    pub fn observe(&self, ident: &Identity, result: &Value) {
        let declared = result.pointer("/capabilities/experimental").and_then(|e| e.get(brand::INBAND_KEY));
        let path = learned_path(ident);
        match declared.map(Manifest::from_value) {
            Some(Ok(m)) => {
                let changed = self.learned.lock().unwrap().insert(ident.id(), m.clone()).as_ref() != Some(&m);
                if changed {
                    tracing::info!(identity = %ident, isolation = %m.isolation, "learned in-band manifest");
                    let _ = std::fs::write(&path, serde_json::to_string_pretty(&m).unwrap_or_default());
                }
            }
            Some(Err(e)) => tracing::warn!(identity = %ident, "server declared an invalid manifest: {e:#}"),
            None => {
                if self.learned.lock().unwrap().remove(&ident.id()).is_some() || path.exists() {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }

    // -------------------------------------------------------------- remote

    fn remote(&self, ident: &Identity) -> Option<(Manifest, String)> {
        if !matches!(ident.kind, Kind::Npm | Kind::Pypi) {
            return None;
        }
        let path = remote_path(ident);
        let entry: Option<RemoteEntry> = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok());
        let fresh = entry.as_ref().is_some_and(|e| {
            let ttl = if e.manifest.is_some() { REMOTE_TTL } else { REMOTE_NEGATIVE_TTL };
            now().saturating_sub(e.fetched_at) < ttl
        });
        if !fresh && self.remote {
            self.fetch_in_background(ident.clone());
        }
        let entry = entry?;
        let m = Manifest::from_value(entry.manifest.as_ref()?).ok()?;
        m.applies_to_version(ident.version.as_deref()).then_some((m, entry.source))
    }

    fn fetch_in_background(&self, ident: Identity) {
        let id = ident.id();
        if !self.fetching.lock().unwrap().insert(id.clone()) {
            return;
        }
        let fetching = self.fetching.clone();
        tokio::spawn(async move {
            let (manifest, source) = fetch_remote(&ident).await.unwrap_or((None, String::new()));
            if let Some(m) = &manifest {
                tracing::info!(identity = %ident, %source, isolation = ?m.get("isolation"), "found published manifest");
            }
            let entry = RemoteEntry { fetched_at: now(), source, manifest };
            let _ = std::fs::write(remote_path(&ident), serde_json::to_string_pretty(&entry).unwrap_or_default());
            fetching.lock().unwrap().remove(&id);
        });
    }
}

fn learned_path(ident: &Identity) -> PathBuf {
    paths::cache().join("learned").join(format!("{}.json", paths::sanitize(&ident.id())))
}

fn remote_path(ident: &Identity) -> PathBuf {
    paths::cache().join("remote").join(format!("{}.json", paths::sanitize(&ident.id())))
}

// ------------------------------------------------------------- overrides

pub fn load_overrides() -> Vec<(PathBuf, Manifest)> {
    let Ok(dir) = std::fs::read_dir(paths::overrides()) else { return vec![] };
    let mut out = vec![];
    for entry in dir.flatten() {
        let p = entry.path();
        if p.extension().is_some_and(|e| e == "json") {
            match std::fs::read_to_string(&p).map_err(anyhow::Error::from).and_then(|t| Manifest::parse(&t)) {
                Ok(m) => out.push((p, m)),
                Err(e) => tracing::warn!("ignoring override {}: {e:#}", p.display()),
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn find_override(ident: &Identity) -> Option<(Manifest, PathBuf)> {
    load_overrides().into_iter().find(|(_, m)| m.matches.iter().any(|p| ident.matches(p))).map(|(p, m)| (m, p))
}

// --------------------------------------------------------------- package

/// Look for a manifest inside the installed package.
pub fn find_in_package(ident: &Identity, home: &Path, program: &Path) -> Option<(Manifest, PathBuf)> {
    match ident.kind {
        Kind::Npm => npx_package_dirs(home, &ident.name, ident.version.as_deref()).into_iter().find_map(|d| manifest_in_dir(&d)),
        Kind::Path => walk_up(Path::new(&ident.name)),
        Kind::Cmd => walk_up(&program.canonicalize().ok()?),
        _ => None,
    }
}

fn walk_up(start: &Path) -> Option<(Manifest, PathBuf)> {
    let mut dir = if start.is_dir() { start } else { start.parent()? };
    for _ in 0..6 {
        if let Some(found) = manifest_in_dir(dir) {
            return Some(found);
        }
        if dir.join("package.json").exists() || dir.join("pyproject.toml").exists() || dir.join("Cargo.toml").exists() {
            return None;
        }
        dir = dir.parent()?;
    }
    None
}

fn manifest_in_dir(dir: &Path) -> Option<(Manifest, PathBuf)> {
    let file = dir.join(brand::MANIFEST_FILE);
    if let Ok(text) = std::fs::read_to_string(&file) {
        match Manifest::parse(&text) {
            Ok(m) => return Some((m, file)),
            Err(e) => tracing::warn!("ignoring invalid {}: {e:#}", file.display()),
        }
    }
    let pkg = dir.join("package.json");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&pkg).ok()?).ok()?;
    let m = Manifest::from_value(v.get(brand::PACKAGE_JSON_KEY)?).ok()?;
    let version = v.get("version").and_then(Value::as_str);
    m.applies_to_version(version).then_some((m, pkg))
}

/// Package directories in the npx cache, best match first.
fn npx_package_dirs(home: &Path, name: &str, version: Option<&str>) -> Vec<PathBuf> {
    let root = home.join(".npm").join("_npx");
    let Ok(entries) = std::fs::read_dir(&root) else { return vec![] };
    let mut found: Vec<(bool, SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let dir = e.path().join("node_modules").join(name);
            let pkg = std::fs::read_to_string(dir.join("package.json")).ok()?;
            let v: Value = serde_json::from_str(&pkg).ok()?;
            let exact = version.is_some() && v.get("version").and_then(Value::as_str) == version;
            if version.is_some() && !exact && semver::Version::parse(version.unwrap()).is_ok() {
                return None;
            }
            let mtime = std::fs::metadata(&dir).and_then(|m| m.modified()).unwrap_or(UNIX_EPOCH);
            Some((exact, mtime, dir))
        })
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    found.into_iter().map(|(_, _, d)| d).collect()
}

// ---------------------------------------------------------------- remote

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(format!("{}/{}", brand::NAME, brand::VERSION))
        .build()
        .expect("http client")
}

/// Fetch a manifest published for this package: npm metadata key first, then
/// `carpool.json` at the root (or package subfolder) of its GitHub repository.
pub async fn fetch_remote(ident: &Identity) -> anyhow::Result<(Option<Value>, String)> {
    let client = http();
    let repo = match ident.kind {
        Kind::Npm => {
            let url = format!(
                "https://registry.npmjs.org/{}/{}",
                ident.name.replace('/', "%2f"),
                ident.version.as_deref().unwrap_or("latest")
            );
            let meta: Value = client.get(&url).send().await?.error_for_status()?.json().await?;
            if let Some(m) = meta.get(brand::PACKAGE_JSON_KEY) {
                if Manifest::from_value(m).is_ok() {
                    return Ok((Some(m.clone()), format!("npm:{}", ident.name)));
                }
            }
            let repo = meta.get("repository");
            let url = repo.and_then(|r| r.get("url").or(Some(r))).and_then(Value::as_str).unwrap_or_default();
            let dir = repo.and_then(|r| r.get("directory")).and_then(Value::as_str).map(String::from);
            github_repo(url).map(|(o, r, sub)| (o, r, dir.or(sub)))
        }
        Kind::Pypi => {
            let url = format!("https://pypi.org/pypi/{}/json", ident.name);
            let meta: Value = client.get(&url).send().await?.error_for_status()?.json().await?;
            let info = &meta["info"];
            let mut urls: Vec<&str> = info["project_urls"].as_object().map(|o| o.values().filter_map(Value::as_str).collect()).unwrap_or_default();
            urls.extend(info["home_page"].as_str());
            urls.into_iter().find_map(github_repo)
        }
        _ => None,
    };
    let Some((owner, name, sub)) = repo else { return Ok((None, String::new())) };
    let mut candidates = vec![];
    if let Some(sub) = sub.as_deref().filter(|s| !s.is_empty()) {
        candidates.push(format!("{}/{}", sub.trim_matches('/'), brand::MANIFEST_FILE));
    }
    candidates.push(brand::MANIFEST_FILE.to_string());
    for path in candidates {
        let url = format!("https://raw.githubusercontent.com/{owner}/{name}/HEAD/{path}");
        let resp = client.get(&url).send().await?;
        if resp.status().is_success() {
            let v: Value = resp.json().await?;
            if Manifest::from_value(&v).is_ok() {
                return Ok((Some(v), format!("github.com/{owner}/{name}/{path}")));
            }
        }
    }
    Ok((None, String::new()))
}

/// `(owner, repo, subdir)` from the many ways package metadata spells a GitHub URL.
pub fn github_repo(url: &str) -> Option<(String, String, Option<String>)> {
    let rest = url
        .trim()
        .trim_start_matches("git+")
        .trim_start_matches("github:")
        .replace("git@github.com:", "https://github.com/")
        .replace("ssh://git@github.com/", "https://github.com/")
        .replace("git://github.com/", "https://github.com/");
    let rest = rest.split("github.com/").nth(1).unwrap_or(if url.starts_with("github:") { &rest } else { "" }).to_string();
    let mut parts = rest.trim_end_matches('/').split('/');
    let owner = parts.next().filter(|s| !s.is_empty())?.to_string();
    let repo = parts.next()?.trim_end_matches(".git").split('#').next()?.to_string();
    let sub = match (parts.next(), parts.next()) {
        (Some("tree" | "blob"), Some(_branch)) => Some(parts.collect::<Vec<_>>().join("/")).filter(|s| !s.is_empty()),
        _ => None,
    };
    Some((owner, repo, sub))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_urls() {
        let r = |u| github_repo(u).map(|(o, n, s)| format!("{o}/{n}/{}", s.unwrap_or_default()));
        assert_eq!(r("git+https://github.com/upstash/context7.git").as_deref(), Some("upstash/context7/"));
        assert_eq!(r("https://github.com/modelcontextprotocol/servers/tree/main/src/time").as_deref(), Some("modelcontextprotocol/servers/src/time"));
        assert_eq!(r("git@github.com:nickclyde/duckduckgo-mcp-server.git").as_deref(), Some("nickclyde/duckduckgo-mcp-server/"));
        assert_eq!(r("github:owner/repo").as_deref(), Some("owner/repo/"));
        assert_eq!(r("https://gitlab.com/a/b"), None);
    }

    #[test]
    fn bundled_registry_is_valid() {
        let all = bundled_manifests();
        assert_eq!(all.len(), bundled::BUNDLED.len(), "every bundled manifest must parse");
        for (file, m) in all {
            assert!(!m.matches.is_empty(), "{file} needs a `match` list");
        }
    }
}
