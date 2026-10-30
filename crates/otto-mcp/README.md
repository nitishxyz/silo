# otto-mcp

MCP client for otto. `Mcp::new(user_config_dir, token_dir)` is an
`otto_tools::ProjectSources`; register it with `Runtime::add_source` (otto passes
`<home>/auth/mcp` as `token_dir`) and hand the same `Arc` to `otto_server::Services`.
Each configured server's tools become otto tools named `mcp__<server>__<tool>`.

## Configuration

Claude Code's format, merged by server name (later wins): user `mcp.json` in otto's
config dir (scope `global`), project `.mcp.json` and `.otto/mcp.json` (scope `project`).

```json
{"mcpServers": {
  "github": {"type": "http", "url": "https://example.com/mcp", "headers": {"X-Team": "${TEAM}"}},
  "files": {"command": "npx", "args": ["-y", "some-server"], "env": {"ROOT": "${HOME:-/tmp}"}, "deferred": true, "timeout": 60000}
}}
```

`type` is `stdio` (default with `command`) or `http`/`streamable-http` (default with
`url`). Strings expand `${VAR}` and `${VAR:-default}`; an unset variable puts that
server in error. Other keys: `cwd` (project servers default to the project root),
`disabled`, `deferred` (unset: defer servers with more than 10 tools), `timeout` (per
call, ms, default 10 min), `startupTimeout` (ms, default 30 s). The user's `mcp.json`
may also hold `disabledProjectServers: {"<root>": ["name"]}`: project servers the user
turned off without editing the shared file.

`Mcp::list(root)` lists servers as written with scope, enabled, `Status` (`Idle`,
`Connecting`, `Connected`, `NeedsSignIn`, `Error`) and tool count, starting enabled
ones. `Mcp::add`/`update`/`remove` edit the files (`settings`), keeping other keys.
`Mcp::subscribe` reports `Event::Status {name, status, tools}` and `Event::Changed`.

## Behavior

- Lazy start: a server starts the first time a session in its project needs tools
  (or `list` shows it). Identical configurations share one server.
- `notifications/{tools,resources,prompts}/list_changed` relists right away; otto
  lists sources before every model request, so changes reach the next step.
- Streamable HTTP: POST answered with JSON or SSE, `Mcp-Session-Id`, and the GET
  notification stream, reopened with `Last-Event-ID` after a drop (the server's
  `retry`, 1 s by default; 5 attempts without an event stop it; `405` means none).
- OAuth (MCP authorization spec): a `401` without a usable token makes the server
  `NeedsSignIn`. `Mcp::login` discovers the authorization server (`WWW-Authenticate`
  `resource_metadata` or `.well-known/oauth-protected-resource`, then RFC 8414 or
  OpenID metadata), registers dynamically, and returns the authorization URL (PKCE
  S256, `resource`, scope) and a loopback port; the redirect, or a pasted callback URL
  (`login_complete`), finishes it. Tokens are stored in an `otto_llm` credential
  store at `token_dir` keyed by server URL, refreshed before expiry and after a `401`.
  A configured `Authorization` header disables OAuth for that server.
- Server requests: `ping`, `roots/list` (the projects the server serves), elicitation
  declined, sampling refused with a JSON-RPC error, others "method not found".
- Resources and prompts: `mcp__<server>__list_resources`, `read_resource {uri}` and
  `get_prompt {name, arguments}` (its description lists the prompts) when the server
  declares the capability; a server tool of the same name wins.
- A crashed server restarts on the next use (3 starts a minute at most); after a
  failed start, calls fail fast for 30 s. Failures are logged once per problem.
- Cancelling a call sends `notifications/cancelled`.
- Effects: `readOnlyHint` tools are read-only; others are `external` and, unless
  `destructiveHint: false`, `write`. HTTP servers add `network`.
- Schemas are made provider-safe (`schema::sanitize`): a top-level object,
  `anyOf`/`oneOf`/`allOf` folded into optional properties, other top-level
  combinators dropped, `required` limited to real properties, dangling `$ref`s
  removed, `items` added to arrays.
- Output: text, images, embedded resources and resource links; other content is
  noted as omitted; `structuredContent` is used when there is no content.

## Limitations

Hand-written JSON-RPC client (protocol `2025-06-18`); the official `rmcp` SDK was
not used because this workspace builds offline. No legacy HTTP+SSE transport, no
sampling, elicitation always declined, prompts are a tool rather than slash commands,
no pre-registered OAuth clients (dynamic registration only).

## Tests

`tests/mcp.rs` runs against `otto-mcp-fixture` (`src/bin/fixture.rs`), a
line-delimited stdio server built with the crate, and checks one session in an
otto runtime with a scripted model. `tests/http.rs` and `tests/oauth.rs` run against
`otto_mcp::fake` (feature `fake`): an in-process HTTP MCP server with an SSE stream
and an OAuth authorization server on a loopback port. `tests/manage.rs` covers
configuration edits. No network beyond loopback.
