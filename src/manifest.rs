//! The manifest an MCP server author ships to declare whether its server may be
//! shared between client sessions, and how it should scale. See spec/SPEC.md.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SPEC_VERSION: &str = "1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Isolation {
    /// One dedicated process per client session. The default for every server
    /// that does not declare otherwise; identical to running it without Carpool.
    Session,
    /// Any session in the same partition may be served by any instance, and a
    /// single instance serves many sessions concurrently.
    Shared,
}

impl std::fmt::Display for Isolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Isolation::Session => "session",
            Isolation::Shared => "shared",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Partition {
    /// Environment variables the server reads. Only these (plus a small
    /// baseline such as PATH and HOME) are passed to shared instances, and
    /// sessions only share an instance when all of their values match.
    /// A trailing `*` matches a prefix, e.g. `DDG_*`.
    #[serde(default)]
    pub env: Vec<String>,
    /// When true, only sessions started from the same working directory share
    /// an instance. Set to false only if the server never uses its cwd.
    #[serde(default = "yes")]
    pub cwd: bool,
}

impl Default for Partition {
    fn default() -> Self {
        Partition { env: vec![], cwd: true }
    }
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Scaling {
    /// Hard cap of in-flight requests one instance may receive at once.
    pub max_concurrent_requests: u32,
    /// Load per instance above which a new instance is started. Defaults to
    /// half of `maxConcurrentRequests`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_concurrent_requests: Option<u32>,
    /// Instances kept running while at least one session is attached.
    pub min_instances: u32,
    /// Upper bound of instances for one partition.
    pub max_instances: u32,
    /// An idle instance above the floor is stopped after this many seconds.
    pub scale_down_after_seconds: u64,
    /// After the last session leaves, the remaining instances stay warm for
    /// this many seconds before the pool scales to zero.
    pub idle_timeout_seconds: u64,
    /// Time allowed for spawn + initialize before an instance is considered failed.
    pub startup_timeout_seconds: u64,
    /// Recycle an instance after it served this many requests (leak protection).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_requests_per_instance: Option<u64>,
    /// Recycle an instance whose resident memory exceeds this many MiB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_memory_mb: Option<u64>,
}

impl Default for Scaling {
    fn default() -> Self {
        Scaling {
            max_concurrent_requests: 16,
            target_concurrent_requests: None,
            min_instances: 0,
            max_instances: 4,
            scale_down_after_seconds: 60,
            idle_timeout_seconds: 300,
            startup_timeout_seconds: 60,
            max_requests_per_instance: None,
            max_memory_mb: None,
        }
    }
}

impl Scaling {
    pub fn target(&self) -> u32 {
        self.target_concurrent_requests
            .unwrap_or_else(|| self.max_concurrent_requests.div_ceil(2))
            .clamp(1, self.max_concurrent_requests.max(1))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(default = "spec_v1")]
    pub spec_version: String,
    pub isolation: Isolation,
    #[serde(default)]
    pub partition: Partition,
    #[serde(default)]
    pub scaling: Scaling,
    /// Semver range of server versions this manifest describes, e.g. ">=1.4.0".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applies_to: Option<String>,
    /// Only used by registries and user overrides: server identities this
    /// manifest applies to, e.g. `npm:@upstash/context7-mcp`, `pypi:mcp-server-time`.
    #[serde(default, rename = "match", skip_serializing_if = "Vec::is_empty")]
    pub matches: Vec<String>,
    /// Free-text justification, shown in `carpool status`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

fn spec_v1() -> String {
    SPEC_VERSION.to_string()
}

const KNOWN_TOP: &[&str] = &["$schema", "specVersion", "isolation", "partition", "scaling", "appliesTo", "match", "reason"];
const KNOWN_PARTITION: &[&str] = &["env", "cwd"];
const KNOWN_SCALING: &[&str] = &[
    "maxConcurrentRequests",
    "targetConcurrentRequests",
    "minInstances",
    "maxInstances",
    "scaleDownAfterSeconds",
    "idleTimeoutSeconds",
    "startupTimeoutSeconds",
    "maxRequestsPerInstance",
    "maxMemoryMb",
];

impl Manifest {
    pub fn session_default() -> Manifest {
        Manifest {
            schema: None,
            spec_version: spec_v1(),
            isolation: Isolation::Session,
            partition: Partition::default(),
            scaling: Scaling::default(),
            applies_to: None,
            matches: vec![],
            reason: None,
        }
    }

    pub fn parse(text: &str) -> Result<Manifest> {
        let value: Value = serde_json::from_str(text).context("manifest is not valid JSON")?;
        Self::from_value(&value)
    }

    pub fn from_value(value: &Value) -> Result<Manifest> {
        let m: Manifest = serde_json::from_value(value.clone()).context("manifest does not match the schema")?;
        m.validate()?;
        Ok(m)
    }

    pub fn validate(&self) -> Result<()> {
        if self.spec_version != SPEC_VERSION {
            bail!("unsupported specVersion {:?} (this runtime understands {:?})", self.spec_version, SPEC_VERSION);
        }
        let s = &self.scaling;
        if s.max_concurrent_requests == 0 {
            bail!("scaling.maxConcurrentRequests must be >= 1");
        }
        if s.max_instances == 0 {
            bail!("scaling.maxInstances must be >= 1");
        }
        if s.min_instances > s.max_instances {
            bail!("scaling.minInstances ({}) exceeds maxInstances ({})", s.min_instances, s.max_instances);
        }
        if let Some(t) = s.target_concurrent_requests {
            if t == 0 || t > s.max_concurrent_requests {
                bail!("scaling.targetConcurrentRequests must be between 1 and maxConcurrentRequests");
            }
        }
        if s.startup_timeout_seconds == 0 {
            bail!("scaling.startupTimeoutSeconds must be >= 1");
        }
        for pat in &self.partition.env {
            let stem = pat.strip_suffix('*').unwrap_or(pat);
            if stem.is_empty() || stem.contains('*') || stem.contains('=') {
                bail!("partition.env entry {pat:?} must be a variable name, optionally ending in '*'");
            }
        }
        if let Some(range) = &self.applies_to {
            semver::VersionReq::parse(range).with_context(|| format!("appliesTo {range:?} is not a semver range"))?;
        }
        Ok(())
    }

    /// Whether this manifest describes the given server version. Unknown
    /// versions are accepted; the caller decides how much to trust that.
    pub fn applies_to_version(&self, version: Option<&str>) -> bool {
        let (Some(range), Some(version)) = (&self.applies_to, version) else {
            return true;
        };
        match (semver::VersionReq::parse(range), semver::Version::parse(version)) {
            (Ok(req), Ok(v)) => req.matches(&v),
            _ => true,
        }
    }

    pub fn env_declared(&self, name: &str) -> bool {
        self.partition.env.iter().any(|pat| match pat.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => name == pat,
        })
    }

    /// Keys the parser ignored; almost always typos worth reporting.
    pub fn unknown_keys(value: &Value) -> Vec<String> {
        let mut out = vec![];
        let check = |obj: Option<&serde_json::Map<String, Value>>, known: &[&str], prefix: &str, out: &mut Vec<String>| {
            if let Some(obj) = obj {
                for k in obj.keys() {
                    if !known.contains(&k.as_str()) {
                        out.push(format!("{prefix}{k}"));
                    }
                }
            }
        };
        check(value.as_object(), KNOWN_TOP, "", &mut out);
        check(value.get("partition").and_then(Value::as_object), KNOWN_PARTITION, "partition.", &mut out);
        check(value.get("scaling").and_then(Value::as_object), KNOWN_SCALING, "scaling.", &mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_shared_manifest_gets_defaults() {
        let m = Manifest::parse(r#"{"isolation":"shared"}"#).unwrap();
        assert_eq!(m.isolation, Isolation::Shared);
        assert!(m.partition.cwd);
        assert_eq!(m.scaling.max_concurrent_requests, 16);
        assert_eq!(m.scaling.target(), 8);
    }

    #[test]
    fn rejects_inconsistent_scaling() {
        assert!(Manifest::parse(r#"{"isolation":"shared","scaling":{"minInstances":5,"maxInstances":2}}"#).is_err());
        assert!(Manifest::parse(r#"{"isolation":"shared","scaling":{"maxConcurrentRequests":0}}"#).is_err());
        assert!(Manifest::parse(r#"{"isolation":"bogus"}"#).is_err());
        assert!(Manifest::parse(r#"{"specVersion":"9","isolation":"shared"}"#).is_err());
    }

    #[test]
    fn env_patterns() {
        let m = Manifest::parse(r#"{"isolation":"shared","partition":{"env":["GITHUB_TOKEN","DDG_*"]}}"#).unwrap();
        assert!(m.env_declared("GITHUB_TOKEN"));
        assert!(m.env_declared("DDG_REGION"));
        assert!(!m.env_declared("GITHUB_TOKEN_2"));
        assert!(Manifest::parse(r#"{"isolation":"shared","partition":{"env":["*"]}}"#).is_err());
    }

    #[test]
    fn version_ranges() {
        let m = Manifest::parse(r#"{"isolation":"shared","appliesTo":">=1.2.0"}"#).unwrap();
        assert!(m.applies_to_version(Some("1.3.0")));
        assert!(!m.applies_to_version(Some("1.1.9")));
        assert!(m.applies_to_version(None));
    }

    #[test]
    fn reports_typos() {
        let v: Value = serde_json::from_str(r#"{"isolation":"shared","scaling":{"maxInstance":3}}"#).unwrap();
        assert_eq!(Manifest::unknown_keys(&v), vec!["scaling.maxInstance"]);
    }
}
