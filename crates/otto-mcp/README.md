# otto-mcp

MCP client for otto. `Mcp::new(user_config_dir)` is an `otto_tools::ProjectSources`;
register it with `Runtime::add_source`. Each configured server's tools become otto
tools named `mcp__<server>__<tool>`.

## Configuration

Claude Code's format, merged by server name (later wins): user `mcp.json` in otto's
config dir, project `.mcp.json`, project `.otto/mcp.json`.

```json
{"mcpServers": {
  "github": {"type": "http", "url": "https://example.com/mcp", "headers": {"Authorization": "Bearer ${GITHUB_TOKEN}"}},
  "files": {"command": "npx", "args": ["-y", "some-server"], "env": {"ROOT": "${HOME:-/tmp}"}, "deferred": true, "timeout": 60000}
}}
```

`type` is `stdio` (default with `command`) or `http`/`streamable-http` (default with
`url`). Strings expand `${VAR}` and `${VAR:-default}`; an unset variable disables
that server with an error in `Mcp::status`. Other keys: `cwd` (project servers
default to the project root), `disabled`, `deferred` (unset: defer servers with
more than 10 tools), `timeout` (per call, ms, default 10 min), `startupTimeout`
(ms, default 30 s).

## Behavior

- Lazy start: a server starts the first time a session in its project needs tools.
  Identical configurations share one server.
- `notifications/tools/list_changed` invalidates the cached list; otto lists
  sources before every model request, so changes reach the next step.
- A crashed server restarts on the next use (3 starts a minute at most); after a
  failed start, calls fail fast for 30 s. Failures are logged once per problem.
- Cancelling a call sends `notifications/cancelled`. Server `ping` requests are
  answered; other server requests get "method not found".
- Effects: `readOnlyHint` tools are read-only; others are `external` and, unless
  `destructiveHint: false`, `write`. HTTP servers add `network`.
- Schemas are made provider-safe (`schema::sanitize`): a top-level object,
  `anyOf`/`oneOf`/`allOf` folded into optional properties, other top-level
  combinators dropped, `required` limited to real properties, dangling `$ref`s
  removed, `items` added to arrays.
- Output: text, images, embedded text resources and resource links; other
  content is noted as omitted; `structuredContent` is used when there is no content.

## Limitations

Hand-written JSON-RPC client (protocol `2025-06-18`); the official `rmcp` SDK was
not used because this workspace builds offline. No OAuth (use `headers`), no
streamable-HTTP GET stream (server notifications arrive only inside POST
responses), no legacy SSE transport, no resources or prompts, no roots or sampling.

## Tests

`tests/mcp.rs` runs against `otto-mcp-fixture` (`src/bin/fixture.rs`), a
line-delimited stdio server built with the crate, and checks one session in an
otto runtime with a scripted model. No network.
