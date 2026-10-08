# Carpool

**Run 100 agent sessions without running 100 copies of every MCP server.**

Claude Code, Codex and every other MCP client start one server process per server, per session. Ten agents with five MCP servers means fifty processes; a hundred agents means five hundred, nearly all of them identical and idle. That memory and CPU go nowhere.

Carpool is a small Rust runtime that sits between your agents and your MCP servers. Servers whose authors declare them safe to share are **pooled and autoscaled** across sessions, Kubernetes-style. Every other server keeps **one private process per session**, exactly as today. Nothing is ever shared on a guess.

```
100 sessions × mcp-server-time      without Carpool: 100 processes · 5,054 MB · 1,000 calls in 8.2 s
                                    with Carpool:      2 processes ·   124 MB · 1,000 calls in 0.65 s
100 sessions × context7 (Node)      without Carpool: ≈ 22 GB (221 MB per session)
                                    with Carpool:      4 processes, autoscaled · ≈ 1 GB
10 Claude Code agents × 3 servers   30 server processes → 14
```

<sub>Apple M-series, 16 GB, `carpool bench` and real `claude -p` sessions. The extra cost per session is a 2.6 MB shim.</sub>

## Quick start

```bash
# install: prebuilt binary for macOS / Linux…
curl -fsSL https://raw.githubusercontent.com/grollat/carpool/main/install.sh | sh
# …or from source
cargo install --git https://github.com/grollat/carpool

# route the MCP servers you already use through Carpool
carpool wrap --client claude      # Claude Code: user, project and .mcp.json servers
carpool wrap --client codex       # Codex: ~/.codex/config.toml

# or add new ones
carpool add time -- uvx mcp-server-time
carpool add context7 --registry io.github.upstash/context7      # straight from the MCP Registry

# watch it work
carpool status --watch
```

There's nothing to start. The daemon launches with the first session and keeps itself running.

```
carpool 0.1.0 · daemon pid 85761 · up 5s

  20 sessions  →  1 server processes  (88 MB RSS)
  without carpool: 20 processes  ·  saved ≈ 19 processes, ≈ 1674 MB

SHARED POOLS
  time  [legacy]  20 sessions · 1/2 instances · 0 queued · community registry mcp-server-time.json
    pid 85781 ready     in-flight 0/16 served 0   88 MB  5s
```

## Why servers must opt in

Whether a server is safe to share can't be inferred from the outside. A server may look stateless and still keep conversation history (sequential-thinking), drive one browser (Playwright), or swap its allowed directories for the first client's roots (filesystem). We audited more than 100 public MCP servers: only the author knows.

So the author declares it, in a `carpool.json` at the repository root:

```json
{
  "specVersion": "1",
  "isolation": "shared",
  "reason": "Stateless documentation lookups.",
  "partition": { "env": ["CONTEXT7_API_KEY"], "cwd": false },
  "scaling": { "maxConcurrentRequests": 8, "maxInstances": 4 }
}
```

* **`isolation`**: `session` (the default, one process per session) or `shared`.
* **`partition.env`**: the variables the server reads. Sessions with different values (e.g. different API tokens) **never** share a process, and undeclared variables never reach a shared process.
* **`partition.cwd`**: share only between sessions in the same directory (default `true`).
* **`scaling`**: per-instance concurrency, min/max instances, and scale-down and idle timeouts.

**Maintainers:** [add a manifest to your server in 2 minutes →](docs/add-a-manifest.md) · [full specification →](spec/SPEC.md)

Carpool ships a [community registry](registry/) of manifests for popular servers until their authors adopt the file: time, fetch, git, context7, duckduckgo, wikipedia, apple-docs and calculator are shared; sequential-thinking, playwright, filesystem, memory and everything are explicitly per-session, each with its reason.

## What Carpool guarantees

* **Safe by default.** No manifest means one process per session, with the session's full environment and working directory: byte-for-byte what your client would have run.
* **Strict partitions.** Never shares across OS users, credentials (`partition.env`), working directories (unless declared irrelevant), commands, or arguments.
* **Correct multiplexing.** JSON-RPC ids and progress tokens are rewritten per session, cancellations are translated, and subscriptions are reference-counted. Shared instances are initialized with no client capabilities, so they can't send a sampling or elicitation request to the wrong agent. Both the classic `initialize` handshake and the stateless 2026-07-28 protocol are supported.
* **Autoscaling.** Requests are bin-packed onto instances. Carpool scales up with the queue, scales down after idle, scales to zero when unused, and recycles instances on memory or request-count limits. A crashed instance fails only its own requests and is replaced.
* **Operator control.** `carpool run --isolate` and overrides in `~/.carpool/manifests/` let you force any decision.
* **Never in the way.** If the daemon is unreachable, `carpool run` executes the server directly.

## Commands

| | |
|---|---|
| `carpool run [--name N] [--manifest F] [--isolate] -- CMD…` | the proxy your client launches |
| `carpool add NAME [-c claude\|codex] [-s user\|local\|project] [-e K=V] (-- CMD… \| --registry ID)` | add a server to Claude Code / Codex through Carpool |
| `carpool wrap` / `carpool unwrap` `[-c claude\|codex] [--dry-run]` | route existing client configs through Carpool, or undo it |
| `carpool search QUERY` | search the official MCP Registry |
| `carpool status [--watch] [--json]` | pools, instances, sessions, memory saved |
| `carpool manifest check FILE` · `init [--shared]` · `explain -- CMD…` · `list` | work with manifests |
| `carpool bench -n 100 -k 10 [--direct] -- CMD…` | load-test a server through Carpool, or directly for comparison |
| `carpool stop` · `carpool logs [NAME]` | stop the daemon and all servers · log locations |

## Docs

* [Add a manifest to your server](docs/add-a-manifest.md)
* [Manifest specification v1](spec/SPEC.md) · [JSON Schema](schema/carpool.schema.json)
* [Claude Code, Codex and orchestrators](docs/clients.md)
* [Architecture](docs/architecture.md)

## Build from source

```bash
git clone https://github.com/grollat/carpool && cd carpool
cargo build --release     # target/release/carpool
cargo test                # unit + end-to-end tests (real daemon, shims and a mock MCP server)
```

macOS and Linux. License: MIT OR Apache-2.0.
