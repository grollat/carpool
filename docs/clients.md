# Using Carpool with Claude Code, Codex and orchestrators

Carpool works as a **proxy command**: instead of launching `npx -y some-server`, your client launches `carpool run -- npx -y some-server`. The first such command starts the Carpool daemon in the background; later ones attach to it.

## Claude Code

### Add a server

```bash
carpool add context7 --client claude -- npx -y @upstash/context7-mcp
carpool add github   --client claude -e GITHUB_PERSONAL_ACCESS_TOKEN=ghp_xxx -- \
      docker run -i --rm -e GITHUB_PERSONAL_ACCESS_TOKEN ghcr.io/github/github-mcp-server
carpool add ctx7     --client claude --registry io.github.upstash/context7   # from the MCP Registry
```

`--scope user` (default) | `local` | `project` map to Claude Code's scopes. User and local scopes go through `claude mcp add-json`; project scope writes `.mcp.json`.

What gets written is ordinary Claude Code config:

```json
{
  "type": "stdio",
  "command": "/usr/local/bin/carpool",
  "args": ["run", "--name", "context7", "--", "npx", "-y", "@upstash/context7-mcp"]
}
```

so you can also add it by hand:

```bash
claude mcp add context7 -- carpool run --name context7 -- npx -y @upstash/context7-mcp
```

### Route everything you already have through Carpool

```bash
carpool wrap --client claude --dry-run   # preview
carpool wrap --client claude             # user, local (this project) and .mcp.json servers
carpool unwrap --client claude           # undo
```

HTTP and SSE servers are left alone: they are already shared by nature.

### Headless and orchestrated sessions

For `claude -p` or an app that spawns many sessions, pass the config explicitly:

```bash
claude -p "…" --mcp-config carpool-mcp.json --strict-mcp-config
```

```json
{ "mcpServers": {
    "time":     { "type": "stdio", "command": "carpool", "args": ["run", "--name", "time", "--", "uvx", "mcp-server-time"] },
    "thinking": { "type": "stdio", "command": "carpool", "args": ["run", "--name", "thinking", "--", "npx", "-y", "@modelcontextprotocol/server-sequential-thinking"] }
}}
```

**Subagents** (the Task tool) reuse their parent session's MCP connections, so they never multiply servers. Separate sessions do, which is exactly what Carpool pools.

## Codex

```bash
carpool add context7 --client codex -- npx -y @upstash/context7-mcp
carpool wrap --client codex       # rewrite every stdio server in ~/.codex/config.toml
carpool unwrap --client codex
```

Carpool edits `$CODEX_HOME/config.toml` (or `.codex/config.toml` with `--scope project`) in place. It keeps your comments and every other key (`env_vars`, `tool_timeout_sec`, `enabled_tools`, …), and sets `startup_timeout_sec = 60` on new entries because a first `npx` download can exceed Codex's 10 s default.

```toml
[mcp_servers.context7]
command = "/usr/local/bin/carpool"
args = ["run", "--name", "context7", "--", "npx", "-y", "@upstash/context7-mcp"]
startup_timeout_sec = 60
```

Equivalent by hand:

```bash
codex mcp add context7 -- carpool run --name context7 -- npx -y @upstash/context7-mcp
```

Codex only passes a whitelisted environment to MCP servers (plus `env` and `env_vars`). Carpool sees exactly that environment, so wrapped servers get the same variables as before.

## Any other client

Replace `command` + `args` with `carpool` + `["run", "--", command, ...args]`. Every stdio MCP client works.

## `carpool run` options

| Option | Effect |
|---|---|
| `--name NAME` | Display name in `carpool status` and log file name. |
| `--manifest FILE` | Use this manifest (operator decision; overrides what the server declares). |
| `--isolate` | Never share this server, whatever it declares. |

If the daemon cannot be reached, `carpool run` executes the server directly, so the client keeps working.

## For orchestrators (e.g. a cockpit managing 100+ sessions)

* Generate one `--mcp-config` per session with `carpool run` commands. Sessions with the same command, args, declared env and cwd share pools automatically.
* `carpool status --json` returns pools, instances (pid, in-flight, served, RSS) and totals, including `processes_without_carpool` and `est_rss_saved_mb`, ready for a dashboard.
* Set machine-wide limits in `~/.carpool/config.toml`:

  ```toml
  max_shared_instances = 256   # cap across all pools
  remote_discovery = true      # look up manifests published in package repositories
  ```
* Force decisions per server with an override in `~/.carpool/manifests/`:

  ```json
  { "match": ["npm:@modelcontextprotocol/server-memory"], "isolation": "shared",
    "reason": "Our agents intentionally share one knowledge graph.", "partition": { "cwd": false } }
  ```
