//! Derive a stable identity (`npm:@scope/pkg`, `pypi:pkg`, `oci:image`, ...)
//! from the command line a client uses to launch an MCP server. The identity
//! is what manifests are looked up and cached by.

use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Npm,
    Pypi,
    Oci,
    Path,
    Cmd,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Npm => "npm",
            Kind::Pypi => "pypi",
            Kind::Oci => "oci",
            Kind::Path => "path",
            Kind::Cmd => "cmd",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Identity {
    pub kind: Kind,
    pub name: String,
    pub version: Option<String>,
}

impl fmt::Display for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.kind.as_str(), self.name)
    }
}

impl Identity {
    pub fn id(&self) -> String {
        self.to_string()
    }

    /// `pattern` is `kind:name`, optionally ending in `*` for a prefix match.
    pub fn matches(&self, pattern: &str) -> bool {
        let id = self.id();
        match pattern.strip_suffix('*') {
            Some(prefix) => id.starts_with(prefix),
            None => id == pattern || format!("{}@{}", id, self.version.as_deref().unwrap_or("")) == pattern,
        }
    }

    pub fn from_command(command: &str, args: &[String], cwd: &Path) -> Identity {
        let base = Path::new(command).file_name().and_then(|s| s.to_str()).unwrap_or(command);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match base {
            "npx" | "bunx" | "pnpx" => npm(&args),
            "pnpm" | "yarn" | "bun" if args.first().is_some_and(|a| *a == "dlx" || *a == "x") => npm(&args[1..]),
            "npm" if args.first().is_some_and(|a| *a == "exec" || *a == "x") => npm(&args[1..]),
            "uvx" => pypi(&args),
            "uv" if args.len() >= 2 && args[0] == "tool" && args[1] == "run" => pypi(&args[2..]),
            "pipx" if args.first() == Some(&"run") => pypi(&args[1..]),
            "docker" | "podman" if args.first() == Some(&"run") => oci(&args[1..]),
            "node" | "deno" | "bun" | "tsx" | "ts-node" | "python" | "python3" | "ruby" => script(&args, cwd)
                .unwrap_or_else(|| cmd(command, cwd)),
            _ => cmd(command, cwd),
        }
    }
}

fn split_version(spec: &str, sep: char) -> (String, Option<String>) {
    // `@scope/pkg@1.2.3` splits on the last '@' that is not at position 0.
    match spec.rfind(sep) {
        Some(i) if i > 0 => (spec[..i].to_string(), Some(spec[i + sep.len_utf8()..].to_string())),
        _ => (spec.to_string(), None),
    }
}

fn npm(args: &[&str]) -> Identity {
    let mut i = 0;
    let mut package: Option<&str> = None;
    while i < args.len() {
        let a = args[i];
        if let Some(p) = a.strip_prefix("--package=") {
            package = Some(p);
        } else if a == "-p" || a == "--package" {
            package = args.get(i + 1).copied();
            i += 1;
        } else if a == "--" {
            i += 1;
            break;
        } else if !a.starts_with('-') {
            break;
        }
        i += 1;
    }
    let spec = package.or_else(|| args.get(i).copied()).unwrap_or("unknown");
    let (name, version) = split_version(spec, '@');
    let version = version.filter(|v| v != "latest");
    Identity { kind: Kind::Npm, name, version }
}

fn pypi(args: &[&str]) -> Identity {
    let mut i = 0;
    let mut from: Option<&str> = None;
    while i < args.len() {
        let a = args[i];
        if let Some(p) = a.strip_prefix("--from=") {
            from = Some(p);
        } else if a == "--from" {
            from = args.get(i + 1).copied();
            i += 1;
        } else if matches!(a, "--with" | "--python" | "-p" | "--index-url" | "--extra-index-url" | "--index") {
            i += 1;
        } else if !a.starts_with('-') {
            break;
        }
        i += 1;
    }
    let spec = from.or_else(|| args.get(i).copied()).unwrap_or("unknown");
    let (name, version) = if spec.contains("==") {
        let (n, v) = spec.split_once("==").unwrap();
        (n.to_string(), Some(v.to_string()))
    } else {
        split_version(spec, '@')
    };
    let name = name.split('[').next().unwrap_or(&name).to_lowercase().replace('_', "-");
    Identity { kind: Kind::Pypi, name, version }
}

const DOCKER_VALUE_FLAGS: &[&str] = &[
    "-e", "--env", "--env-file", "-v", "--volume", "--name", "--network", "--net", "-p", "--publish", "--mount", "-w",
    "--workdir", "-u", "--user", "--entrypoint", "-l", "--label", "--platform", "--pull", "--add-host", "--cpus", "-m",
    "--memory", "--hostname", "-h", "--dns", "--cap-add", "--cap-drop", "--security-opt", "--tmpfs", "--ulimit",
    "--log-driver", "--log-opt", "--restart", "--runtime", "--shm-size", "--stop-signal", "--device",
];

fn oci(args: &[&str]) -> Identity {
    let mut i = 0;
    while i < args.len() {
        let a = args[i];
        if a.starts_with('-') {
            if !a.contains('=') && DOCKER_VALUE_FLAGS.contains(&a) {
                i += 1;
            }
            i += 1;
            continue;
        }
        let (name, version) = match a.rsplit_once(':') {
            Some((n, v)) if !v.contains('/') => (n.to_string(), Some(v.to_string())),
            _ => (a.to_string(), None),
        };
        return Identity { kind: Kind::Oci, name, version: version.filter(|v| v != "latest") };
    }
    Identity { kind: Kind::Oci, name: "unknown".into(), version: None }
}

fn script(args: &[&str], cwd: &Path) -> Option<Identity> {
    let mut i = 0;
    while i < args.len() {
        let a = args[i];
        if a == "-m" {
            // `python -m package.module`
            let module = args.get(i + 1)?;
            let root = module.split('.').next().unwrap_or(module).replace('_', "-");
            return Some(Identity { kind: Kind::Pypi, name: root.to_lowercase(), version: None });
        }
        if !a.starts_with('-') {
            let p = absolutize(a, cwd);
            return Some(Identity { kind: Kind::Path, name: p.display().to_string(), version: None });
        }
        i += 1;
    }
    None
}

fn cmd(command: &str, cwd: &Path) -> Identity {
    if command.contains('/') {
        let p = absolutize(command, cwd);
        return Identity { kind: Kind::Path, name: p.display().to_string(), version: None };
    }
    Identity { kind: Kind::Cmd, name: command.to_string(), version: None }
}

fn absolutize(p: &str, cwd: &Path) -> PathBuf {
    let path = Path::new(p);
    let joined = if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) };
    joined.canonicalize().unwrap_or(joined)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(cmd: &str) -> Identity {
        let mut parts = cmd.split_whitespace();
        let c = parts.next().unwrap();
        let args: Vec<String> = parts.map(String::from).collect();
        Identity::from_command(c, &args, Path::new("/tmp"))
    }

    #[test]
    fn npm_forms() {
        assert_eq!(id("npx -y @upstash/context7-mcp").id(), "npm:@upstash/context7-mcp");
        let i = id("npx -y @upstash/context7-mcp@1.2.3 --api-key x");
        assert_eq!((i.id(), i.version.as_deref()), ("npm:@upstash/context7-mcp".into(), Some("1.2.3")));
        assert_eq!(id("npx @playwright/mcp@latest").version, None);
        assert_eq!(id("npx --yes -p mcp-remote mcp-remote http://x").id(), "npm:mcp-remote");
        assert_eq!(id("/opt/homebrew/bin/npx -y foo").id(), "npm:foo");
        assert_eq!(id("pnpm dlx foo@2").version.as_deref(), Some("2"));
    }

    #[test]
    fn pypi_forms() {
        assert_eq!(id("uvx mcp-server-time --local-timezone UTC").id(), "pypi:mcp-server-time");
        assert_eq!(id("uvx mcp-server-fetch==2025.1.1").version.as_deref(), Some("2025.1.1"));
        assert_eq!(id("uvx --from duckduckgo-mcp-server duckduckgo-mcp-server").id(), "pypi:duckduckgo-mcp-server");
        assert_eq!(id("uv tool run Wikipedia_MCP").id(), "pypi:wikipedia-mcp");
        assert_eq!(id("python -m mcp_server_time").id(), "pypi:mcp-server-time");
    }

    #[test]
    fn oci_forms() {
        let i = id("docker run -i --rm -e GITHUB_PERSONAL_ACCESS_TOKEN ghcr.io/github/github-mcp-server");
        assert_eq!(i.id(), "oci:ghcr.io/github/github-mcp-server");
        assert_eq!(id("docker run -i --rm mcp/time:1.0").version.as_deref(), Some("1.0"));
        assert_eq!(id("docker run --env=A=b -i localhost:5000/img").id(), "oci:localhost:5000/img");
    }

    #[test]
    fn matching() {
        let i = id("npx -y @upstash/context7-mcp@1.2.3");
        assert!(i.matches("npm:@upstash/context7-mcp"));
        assert!(i.matches("npm:@upstash/*"));
        assert!(i.matches("npm:@upstash/context7-mcp@1.2.3"));
        assert!(!i.matches("npm:@upstash/other"));
    }
}
