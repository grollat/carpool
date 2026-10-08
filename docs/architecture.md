# How Carpool works

```
 Claude Code / Codex sessions                   carpool daemon (one per user)                  MCP servers
 ────────────────────────────                   ───────────────────────────                  ───────────
 session 1 ─ carpool run ─┐                       ┌─ manifest discovery
 session 2 ─ carpool run ─┤   Unix socket         │
 session 3 ─ carpool run ─┼──────────────────────►├─ pool context7 [shared] ─── autoscaler ──► 2 × context7
   …                    │   (~2.6 MB per shim)  ├─ pool time     [shared] ──────────────────► 1 × time
 session 100 ─ carpool run┘                       └─ session pipes [session] ─────────────────► 100 × playwright
```

## Components

**`carpool run`** (the shim) is what your client launches. It is a small synchronous program (about 2.6 MB resident) that connects to the daemon, sends a hello (command, args, environment, cwd), and then relays bytes. If the daemon is not running it starts it; if it cannot reach the daemon it `exec`s the server directly.

**The daemon** (`carpool daemon`, started automatically) listens on `~/.carpool/run/daemon.sock` (mode 0600). For each session it:

1. derives the server **identity** from the command line (`npm:@upstash/context7-mcp`, `pypi:mcp-server-time`, `oci:ghcr.io/...`, `path:/abs/script.js`);
2. **resolves a manifest**, in this order: `--isolate` → operator override in `~/.carpool/manifests/` → `--manifest` → manifest in the installed package → manifest the server declared in-band earlier → manifest published in the package's repository or npm metadata (fetched in the background, cached 24 h) → bundled community registry → **default `session`**;
3. runs the session **dedicated** (`session`) or attaches it to a **pool** (`shared`).

## Dedicated sessions

One process per session, with the session's full environment and cwd, bytes relayed untouched. This is identical to running without Carpool, plus process-group cleanup (no orphaned `npx` children) and a status view. The daemon reads the server's handshake result to learn any in-band manifest for next time.

## Pools

A pool serves one **partition**: same resolved command and args, same values of the manifest's `partition.env` variables (plus a fixed baseline such as `PATH` and `HOME`), same cwd if `partition.cwd`, and the same handshake flavour (protocol version and client implementation for `initialize`-based servers, or stateless).

Each pool is a single task that owns all of its state, so there are no locks on the hot path.

* **Handshake.** For `initialize`-based servers the pool initializes each instance itself, declaring **no client capabilities**, so the server can never send a sampling, elicitation or roots request that cannot be attributed to one session. Each session's `initialize` is answered from the cached result. Stateless (2026-07-28+) requests are self-describing and are forwarded as they are.
* **Multiplexing.** Request ids and progress tokens are rewritten to pool-unique values and mapped back. `notifications/cancelled` is translated. A disconnecting session's in-flight requests are cancelled. Subscriptions are reference-counted. `logging/setLevel` is acknowledged but not forwarded.
* **Routing.** Bin packing: fill the oldest instance up to `targetConcurrentRequests`, then spill to the least-loaded instance below `maxConcurrentRequests`, and queue beyond that. Surplus instances therefore go idle and can be reclaimed.
* **Autoscaling.**
  * *Scale up*: start enough instances to absorb the queue at target load, or one more when every instance is above target. Bounded by `maxInstances` and the machine-wide `max_shared_instances`.
  * *Scale down*: an idle instance above the floor is stopped after `scaleDownAfterSeconds`. After the last session leaves, the pool stays warm for `idleTimeoutSeconds`, then scales to zero.
  * *Recycle*: instances above `maxMemoryMb` or past `maxRequestsPerInstance` are drained and replaced.
  * *Failures*: crashed instances fail only their own in-flight requests and are replaced. Start-up failures back off exponentially; after three, waiting sessions get the error.
* **Shutdown.** Close stdin, wait 2 s, SIGTERM the process group, wait 3 s, then SIGKILL, as the MCP stdio transport recommends.

## Protocol versions

Claude Code ≥ 2.1 probes servers with the stateless `server/discover` first and falls back to `initialize`. Python-SDK servers older than 2026 ignore the probe rather than rejecting it. Once any session has fallen back, Carpool records that the server is pre-2026 (persisted for 24 h) and answers later probes immediately.

## Files

```
~/.carpool/
  run/daemon.sock        socket (0600)
  logs/daemon.log        daemon log
  logs/servers/*.log     stderr of shared instances (and dedicated ones)
  manifests/*.json       operator overrides (with "match")
  cache/learned/         manifests servers declared in-band
  cache/remote/          manifests found in package repositories
  config.toml            max_shared_instances, remote_discovery
```

`CARPOOL_HOME` relocates all of it. `CARPOOL_LOG=debug` makes the daemon verbose.
