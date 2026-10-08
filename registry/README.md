# Community manifest registry

Manifests for popular MCP servers whose authors have not shipped their own `carpool.json` yet. They are compiled into the `carpool` binary and used only when no better source exists: an operator override, the server's own package, its in-band declaration, or its repository all take precedence.

Each file needs a `match` list of server identities (`npm:<package>`, `pypi:<package>`, `oci:<image>`) and a `reason`.

**Adding one:** read the server's source and answer the four questions in [docs/add-a-manifest.md](../docs/add-a-manifest.md). Open a PR with the manifest and a short note on what you checked: in-memory state, concurrency, environment variables, cwd use, and server-to-client requests. When in doubt, use `"isolation": "session"`.

**Better still:** send the manifest to the server's own repository. Once it ships there, remove it from here.
