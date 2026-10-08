# Add a manifest to your MCP server (2 minutes)

Your users run many agent sessions in parallel, and each session starts its own copy of your server. If your server is safe to share, one file lets runtimes like Carpool serve all of those sessions from one or a few processes.

## 1. Decide: `shared` or `session`?

Answer four questions about your server:

| | Question | If "no" |
|---|---|---|
| 1 | Does every request produce the same result no matter which client sent the earlier requests? (Caches of public or token-scoped data are fine. History, open browsers, a "current directory" set by a tool, and per-client sessions are not.) | `session` |
| 2 | Can it handle several requests at the same time? | `shared`, with `maxConcurrentRequests: 1` |
| 3 | Does it work when the client declares no `roots`, `sampling` or `elicitation` support? | `session` |
| 4 | Do you know every environment variable it reads? | find out first |

Four "yes" answers mean `shared`.

## 2. Add `carpool.json` to your repository root

```json
{
  "$schema": "https://raw.githubusercontent.com/grollat/carpool/main/schema/carpool.schema.json",
  "specVersion": "1",
  "isolation": "shared",
  "reason": "Stateless: every tool call is independent.",
  "partition": {
    "env": ["MY_SERVER_API_KEY"],
    "cwd": false
  },
  "scaling": { "maxConcurrentRequests": 8, "maxInstances": 4 }
}
```

* **`partition.env`**: list **every** environment variable your server reads, credentials above all. Sessions with different values never share a process. Undeclared variables are not passed to shared instances at all. `PREFIX_*` matches a prefix.
* **`partition.cwd`**: keep `true` (the default) if relative paths, `.env` files or "the current repository" matter. Then only sessions in the same directory share.
* **`scaling`**: everything is optional. Set `maxConcurrentRequests: 1` if a request can block your event loop; the runtime will scale out instead.

In a monorepo, put it in the package's folder (the one in `repository.directory` / the registry's `subfolder`).

That's it. Runtimes find the file from your package's repository URL.

### Optional: ship it inside the package

* **npm**: add a `"carpool": { ... }` key to `package.json` (no `files` change needed), or add `carpool.json` to `files`. This pins the manifest to the exact installed version.
* **Any language**: declare it in-band in your initialize result:

  ```ts
  // TypeScript SDK
  new McpServer({ name: "my-server", version: "1.0.0" }, {
    capabilities: { experimental: { "carpool/manifest": { isolation: "shared", partition: { cwd: false } } } },
  });
  ```
  ```python
  # Python SDK (low-level server)
  options = server.create_initialization_options(experimental_capabilities={
      "carpool/manifest": {"isolation": "shared", "partition": {"cwd": False}}
  })
  await server.run(read_stream, write_stream, options)
  ```

## 3. Check it

```bash
carpool manifest check carpool.json
carpool bench -n 50 -k 10 --manifest carpool.json -- npx -y your-server   # 50 sessions through a shared pool
```

## Examples

| Server type | Manifest |
|---|---|
| Public API, no auth | `{"isolation":"shared","partition":{"cwd":false}}` |
| Token-scoped API (GitHub, Linear, …) | `{"isolation":"shared","partition":{"env":["GITHUB_PERSONAL_ACCESS_TOKEN"],"cwd":false}}` |
| Git / file tools relative to the project | `{"isolation":"shared","partition":{"cwd":true}}` |
| CPU-bound / blocking | `{"isolation":"shared","scaling":{"maxConcurrentRequests":1,"maxInstances":8}}` |
| Browser automation, conversation memory | `{"isolation":"session","reason":"…"}` |

Declaring `session` explicitly is useful too: users see *why* your server needs one process per session.

Full details are in [the specification](../spec/SPEC.md).
