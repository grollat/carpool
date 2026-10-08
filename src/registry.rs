//! The official MCP Registry (registry.modelcontextprotocol.io): search, and
//! turn a server.json into a runnable command. Any server from the registry
//! runs under Carpool; without a manifest it simply gets `isolation: session`.

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::discovery::bundled_manifests;
use crate::identity::Identity;

const API: &str = "https://registry.modelcontextprotocol.io/v0.1";

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(format!("{}/{}", crate::brand::NAME, crate::brand::VERSION))
        .build()
        .expect("http client")
}

pub struct Resolved {
    pub command: String,
    pub args: Vec<String>,
    pub required_env: Vec<String>,
}

fn args_of(list: Option<&Value>) -> Vec<String> {
    let mut out = vec![];
    for a in list.and_then(Value::as_array).into_iter().flatten() {
        let value = a.get("value").or_else(|| a.get("default")).and_then(Value::as_str);
        match (a.get("type").and_then(Value::as_str), a.get("name").and_then(Value::as_str), value) {
            (Some("named"), Some(name), v) => {
                out.push(name.to_string());
                if let Some(v) = v {
                    out.push(v.to_string());
                }
            }
            (_, _, Some(v)) => out.push(v.to_string()),
            _ => {}
        }
    }
    out
}

/// Pick the best stdio package of a server.json and build its launch command.
pub fn command_for(server: &Value) -> Result<Resolved> {
    let packages = server.get("packages").and_then(Value::as_array).cloned().unwrap_or_default();
    let stdio = |p: &&Value| p.pointer("/transport/type").and_then(Value::as_str).unwrap_or("stdio") == "stdio";
    let rank = |p: &Value| match p.get("registryType").and_then(Value::as_str) {
        Some("npm") => 0,
        Some("pypi") => 1,
        Some("oci") => 2,
        _ => 9,
    };
    let mut candidates: Vec<&Value> = packages.iter().filter(stdio).collect();
    candidates.sort_by_key(|p| rank(p));
    let Some(p) = candidates.first() else {
        if server.get("remotes").and_then(Value::as_array).is_some_and(|r| !r.is_empty()) {
            bail!("this server is remote-only (HTTP); add it to your client directly, it is already shared by nature");
        }
        bail!("no stdio package in this server.json");
    };
    let id = p.get("identifier").and_then(Value::as_str).context("package without identifier")?;
    let version = p.get("version").and_then(Value::as_str);
    let env: Vec<&Value> = p.get("environmentVariables").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default();
    let required_env = env
        .iter()
        .filter(|e| e.get("isRequired").and_then(Value::as_bool).unwrap_or(false))
        .filter_map(|e| e.get("name").and_then(Value::as_str).map(String::from))
        .collect();
    let runtime_args = args_of(p.get("runtimeArguments"));
    let package_args = args_of(p.get("packageArguments"));
    let (command, mut args) = match p.get("registryType").and_then(Value::as_str) {
        Some("npm") => {
            let mut a = if runtime_args.is_empty() { vec!["-y".to_string()] } else { runtime_args };
            a.push(version.map(|v| format!("{id}@{v}")).unwrap_or(id.to_string()));
            (p.get("runtimeHint").and_then(Value::as_str).unwrap_or("npx").to_string(), a)
        }
        Some("pypi") => {
            let mut a = runtime_args;
            a.push(version.map(|v| format!("{id}=={v}")).unwrap_or(id.to_string()));
            (p.get("runtimeHint").and_then(Value::as_str).unwrap_or("uvx").to_string(), a)
        }
        Some("oci") => {
            let mut a = vec!["run".to_string(), "-i".into(), "--rm".into()];
            a.extend(runtime_args.into_iter().filter(|x| x != "run"));
            for e in &env {
                if let Some(n) = e.get("name").and_then(Value::as_str) {
                    a.push("-e".into());
                    a.push(n.into());
                }
            }
            let image = match version {
                Some(v) if !id.contains(':') => format!("{id}:{v}"),
                _ => id.to_string(),
            };
            a.push(image);
            ("docker".to_string(), a)
        }
        other => bail!("unsupported package type {other:?}"),
    };
    args.extend(package_args);
    Ok(Resolved { command, args, required_env })
}

pub async fn fetch(name: &str) -> Result<Value> {
    let url = format!("{API}/servers/{}/versions/latest", name.replace('/', "%2F"));
    let resp = client().get(&url).send().await?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        bail!("{name} is not in the MCP Registry (try `{} search`)", crate::brand::NAME);
    }
    let v: Value = resp.error_for_status()?.json().await?;
    Ok(v.get("server").cloned().unwrap_or(v))
}

pub async fn resolve(name: &str) -> Result<Resolved> {
    command_for(&fetch(name).await?)
}

pub async fn search(query: &str, limit: u32) -> Result<()> {
    let url = format!("{API}/servers?search={}&limit={limit}&version=latest", urlencode(query));
    let v: Value = client().get(&url).send().await?.error_for_status()?.json().await?;
    let servers = v.get("servers").and_then(Value::as_array).cloned().unwrap_or_default();
    if servers.is_empty() {
        println!("no servers matching {query:?} (the registry matches substrings of server names)");
        return Ok(());
    }
    let bundled = bundled_manifests();
    for entry in servers {
        let s = entry.get("server").unwrap_or(&entry);
        let name = s["name"].as_str().unwrap_or("?");
        let desc: String = s["description"].as_str().unwrap_or("").chars().take(70).collect();
        let (kind, isolation) = match command_for(s) {
            Ok(r) => {
                let ident = Identity::from_command(&r.command, &r.args, Path::new("/"));
                let iso = bundled
                    .iter()
                    .find(|(_, m)| m.matches.iter().any(|p| ident.matches(p)))
                    .map(|(_, m)| m.isolation.to_string())
                    .unwrap_or_else(|| "session".into());
                (r.command, iso)
            }
            Err(_) => ("remote".into(), "-".into()),
        };
        println!("{name:<48} {:<10} {kind:<6} {isolation:<8} {desc}", s["version"].as_str().unwrap_or(""));
    }
    println!("\nadd one with: {} add <name> --registry <server name>", crate::brand::NAME);
    Ok(())
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn npm_package() {
        let s = json!({"packages":[{"registryType":"npm","identifier":"@upstash/context7-mcp","version":"1.0.0","transport":{"type":"stdio"},
            "runtimeArguments":[{"type":"positional","value":"-y"}],
            "packageArguments":[{"type":"named","name":"--api-key","value":"x"}],
            "environmentVariables":[{"name":"CONTEXT7_API_KEY","isRequired":true}]}]});
        let r = command_for(&s).unwrap();
        assert_eq!(r.command, "npx");
        assert_eq!(r.args, ["-y", "@upstash/context7-mcp@1.0.0", "--api-key", "x"]);
        assert_eq!(r.required_env, ["CONTEXT7_API_KEY"]);
    }

    #[test]
    fn prefers_npm_then_pypi_then_oci() {
        let s = json!({"packages":[
            {"registryType":"oci","identifier":"ghcr.io/a/b","version":"1","environmentVariables":[{"name":"TOKEN"}]},
            {"registryType":"pypi","identifier":"b-mcp","version":"2"}]});
        let r = command_for(&s).unwrap();
        assert_eq!((r.command.as_str(), r.args.as_slice()), ("uvx", ["b-mcp==2".to_string()].as_slice()));
        let s = json!({"packages":[{"registryType":"oci","identifier":"ghcr.io/a/b","version":"1","environmentVariables":[{"name":"TOKEN"}]}]});
        assert_eq!(command_for(&s).unwrap().args, ["run", "-i", "--rm", "-e", "TOKEN", "ghcr.io/a/b:1"]);
    }

    #[test]
    fn remote_only() {
        assert!(command_for(&json!({"remotes":[{"type":"streamable-http","url":"https://x"}]})).is_err());
    }
}
