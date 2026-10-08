# Carpool Manifest Specification — v1

**Status:** Draft 1 · **File:** `carpool.json` · **Schema:** [`schema/carpool.schema.json`](../schema/carpool.schema.json)

The key words MUST, MUST NOT, SHOULD, SHOULD NOT and MAY are to be interpreted as described in RFC 2119.

## 1. Why this exists

MCP clients such as Claude Code and Codex start one stdio server process per server **per session**. A machine running 100 agent sessions with 5 MCP servers runs 500 server processes, most of them identical, idle copies.

Many of those servers could safely serve every session from one process. But **a runtime cannot infer that from the outside**: a server that looks stateless may cache per-client data, hold a browser open, accumulate conversation history, or replace its configuration with the first client's roots. An analysis of 100+ public MCP servers found no reliable signal short of reading the source. Only the server's author knows.

This specification lets the author say so, in a small JSON document, and tells runtimes exactly what they may and may not do with that statement.

## 2. Terminology

| Term | Meaning |
|---|---|
| **Session** | One client connection to one MCP server, e.g. one Claude Code session's connection to `context7`. |
| **Instance** | One running server process. |
| **Runtime** | Software that starts instances on behalf of sessions, e.g. Carpool. |
| **Partition** | The set of sessions that may be served by the same instances. Sessions in different partitions never share an instance. |
| **Pool** | The instances serving one partition. |

## 3. The manifest

A manifest is a JSON object. Only `isolation` is required.

```json
{
  "$schema": "https://raw.githubusercontent.com/grollat/carpool/main/schema/carpool.schema.json",
  "specVersion": "1",
  "isolation": "shared",
  "reason": "Stateless documentation lookups.",
  "partition": {
    "env": ["CONTEXT7_API_KEY", "CONTEXT7_API_URL"],
    "cwd": false
  },
  "scaling": {
    "maxConcurrentRequests": 8,
    "maxInstances": 4
  }
}
```

### 3.1 Top-level fields

| Field | Type | Default | Description |
|---|---|---|---|
| `specVersion` | string | `"1"` | Version of this specification. Runtimes MUST reject manifests with a version they do not understand and fall back to `session`. |
| `isolation` | `"session"` \| `"shared"` | — (required) | See §4. |
| `partition` | object | see §5 | What must match for two sessions to share. |
| `scaling` | object | see §6 | Capacity hints for the runtime. |
| `appliesTo` | string | — | A semver range of server versions this manifest describes, e.g. `">=2.1.0"`. A runtime that knows the server's version MUST ignore a manifest whose range does not match. |
| `reason` | string | — | One sentence explaining the choice. Shown to operators. Strongly RECOMMENDED. |
| `match` | string[] | — | Only for manifests that live *outside* the server (registries, operator overrides): the server identities it applies to, e.g. `npm:@upstash/context7-mcp`, `pypi:mcp-server-time`, `oci:ghcr.io/github/github-mcp-server`. A trailing `*` matches a prefix. Servers shipping their own manifest SHOULD omit it. |

Unknown fields MUST be ignored by runtimes (forward compatibility). Tooling SHOULD warn about them, since they are usually typos.

## 4. Isolation levels

### 4.1 `session` (default)

Every session gets its own dedicated instance, started for that session and stopped when it ends. This is exactly how MCP clients behave without a runtime.

**A server without a manifest MUST be treated as `session`.** Runtimes MUST NOT infer `shared` from any other signal.

Declare `session` explicitly when your server keeps per-conversation state, so users and registries see the reason.

### 4.2 `shared`

Any session in the partition may be served by any instance in the pool, and one instance serves many sessions concurrently.

By declaring `shared`, the server author asserts **all** of the following:

1. **No cross-session leakage.** The server keeps no in-memory state, created by one client's requests, that changes the result of another client's requests. Caches of data that is identical for every client in the partition (public documentation, a token-scoped API response) are fine. Conversation history, open browsers, "current directory" or "current repository" state set by a tool, and per-client sessions are not.
2. **Concurrency safety.** The server handles up to `scaling.maxConcurrentRequests` requests at once without corrupting its state or files.
3. **No reliance on client-specific capabilities.** The server works correctly when the client declares no capabilities: no `roots`, no `sampling`, no `elicitation`. (Runtimes initialize shared instances that way; see §7.)
4. **Complete partition declaration.** Every environment variable that affects the server's behavior or identity is listed in `partition.env`, and `partition.cwd` is `true` if the working directory affects it.
5. **Global effects are acceptable.** Changes that are process-wide by nature (resource subscriptions, `list_changed` notifications, log output) may be observed by every session in the partition.

If any of these is false for some configuration of your server, declare `session`.

## 5. Partition

Two sessions MAY share an instance only if all of the following are identical:

* the resolved command and its arguments (always; not configurable);
* the values of the variables in `partition.env`, plus a baseline the runtime always passes (`PATH`, `HOME`, `USER`, locale, `TMPDIR`, `TZ`, proxy, TLS-bundle, Docker-context and package-index variables);
* the working directory, when `partition.cwd` is `true`;
* the protocol version and the client implementation (`clientInfo`) used in the handshake, for servers using the `initialize` handshake.

| Field | Type | Default | Description |
|---|---|---|---|
| `env` | string[] | `[]` | Environment variables the server reads. A trailing `*` matches a prefix (`DDG_*`). |
| `cwd` | boolean | `true` | Whether the working directory is part of the partition. Set `false` only if the server never uses its cwd (no relative paths, no `.env` loading, no "current repository"). |

**Environment for shared instances.** A shared instance receives *only* the baseline variables and the variables matching `partition.env`. A variable you forget to declare is therefore missing, which shows up immediately in testing, instead of silently leaking one session's configuration into another. Credentials are the most important case: list every token variable, and sessions with different tokens will never share an instance.

Session instances receive the session's complete environment and working directory, unchanged.

## 6. Scaling

All fields are optional hints. Runtimes SHOULD honor them and MAY apply stricter machine-wide limits.

| Field | Type | Default | Meaning |
|---|---|---|---|
| `maxConcurrentRequests` | integer ≥ 1 | 16 | Hard cap of in-flight requests per instance. Use `1` for servers that block an event loop. |
| `targetConcurrentRequests` | integer | ⌈max/2⌉ | Load per instance above which the runtime adds an instance. |
| `minInstances` | integer ≥ 0 | 0 | Instances kept while at least one session is attached. |
| `maxInstances` | integer ≥ 1 | 4 | Upper bound for one partition. |
| `scaleDownAfterSeconds` | integer | 60 | An idle instance above the floor is stopped after this long. |
| `idleTimeoutSeconds` | integer | 300 | After the last session leaves, instances stay warm this long, then the pool scales to zero. |
| `startupTimeoutSeconds` | integer ≥ 1 | 60 | Time allowed for start-up and handshake. |
| `maxRequestsPerInstance` | integer | — | Recycle an instance after this many requests (leak protection). |
| `maxMemoryMb` | integer | — | Recycle an instance whose resident memory exceeds this. |

Scaling only applies to `shared` servers.

## 7. Runtime obligations

A conforming runtime that shares an instance between sessions MUST:

1. **Never share across partitions**, and never across operating-system users.
2. **Rewrite JSON-RPC request ids** so that equal ids from different sessions cannot collide, and map responses back to the originating session.
3. **Rewrite progress tokens** (`params._meta.progressToken`) the same way, and deliver `notifications/progress` only to the session that owns the token.
4. **Translate `notifications/cancelled`** to the rewritten id. When a session disconnects, cancel its in-flight requests.
5. **Initialize instances that use the `initialize` handshake itself, declaring no client capabilities**, and answer each session's `initialize` with the instance's result. Server-to-client requests other than `ping` MUST NOT be forwarded to any session.
6. **Not forward process-global controls from one session**: `logging/setLevel` is acknowledged without being forwarded.
7. **Reference-count resource subscriptions**, and deliver `notifications/resources/updated` only to subscribed sessions.
8. Broadcast other server notifications (`notifications/*/list_changed`, `notifications/message`) to every session in the pool.
9. For the stateless protocol (2026-07-28 and later), where every request carries its own protocol version and capabilities, forward requests unchanged apart from rules 2–4.

A runtime MUST treat a server as `session` when it cannot find a valid manifest, when the manifest's `specVersion` is unknown, or when `appliesTo` excludes the running version.

## 8. Publishing a manifest

A server MAY publish its manifest in any of these ways. Runtimes SHOULD check them in this order, after any operator override:

1. **A `carpool.json` file in the source repository**, at the root or in the package's subfolder of a monorepo. Runtimes find it from the repository URL in the package metadata (npm `repository`, PyPI `project_urls`). This is the simplest option and works for every ecosystem.
2. **Inside the installed npm package**: a `carpool.json` next to `package.json` (add it to `files`), or a `"carpool"` key in `package.json`, which needs no `files` change and is also visible in the npm registry metadata. This form is exact for the installed version.
3. **In-band**, in the handshake result: `capabilities.experimental["carpool/manifest"]` of `InitializeResult`, or of `DiscoverResult` for the stateless protocol. This works for every language and distribution channel, including containers. Runtimes learn it on first contact, so the first session runs isolated.

When several are present, the installed package wins over in-band, and in-band wins over the repository copy. Repository copies describe the default branch, so authors SHOULD set `appliesTo` when a release changes the declaration.

## 9. Security considerations

* **Trust.** A manifest is a statement by the server's author, about the code you already chose to run with your credentials. Declaring `shared` cannot give the server access to anything it did not already have. The risk is a *wrong* declaration, which can leak data between sessions of the **same** user in the **same** partition. Runtimes therefore never share across OS users or credential partitions, and operators can override any manifest.
* **Credentials.** Declare every credential variable in `partition.env`. Sessions using different tokens then always get different instances.
* **Operator overrides.** Operators MUST be able to force `session` for any server. Runtimes MAY let operators declare `shared` for servers that do not ship a manifest; the operator then takes the author's responsibility.
* **Third-party registries.** Manifests written by someone other than the author (such as Carpool's bundled community registry) SHOULD state their reasoning and SHOULD be replaced by the author's own manifest as soon as one exists.

## 10. Versioning

`specVersion` changes only for incompatible changes. New optional fields can be added within version 1; runtimes ignore fields they do not know.

## Appendix A. Examples

**Stateless public API, no credentials:**
```json
{ "specVersion": "1", "isolation": "shared", "partition": { "cwd": false } }
```

**Token-scoped API.** Sessions share only when they use the same token:
```json
{
  "specVersion": "1",
  "isolation": "shared",
  "reason": "Each call is independent; identity comes only from the token.",
  "partition": { "env": ["GITHUB_PERSONAL_ACCESS_TOKEN", "GITHUB_HOST", "GITHUB_TOOLSETS"], "cwd": false },
  "scaling": { "maxConcurrentRequests": 8, "maxInstances": 4 }
}
```

**Working-directory-relative tool**, shared only within one project:
```json
{ "specVersion": "1", "isolation": "shared", "partition": { "cwd": true } }
```

**Blocking evaluator.** One request per process; scale out instead:
```json
{ "specVersion": "1", "isolation": "shared", "partition": { "cwd": false },
  "scaling": { "maxConcurrentRequests": 1, "maxInstances": 8 } }
```

**Stateful server**, declared so users see why:
```json
{ "specVersion": "1", "isolation": "session",
  "reason": "Keeps one browser and its tabs per process." }
```
