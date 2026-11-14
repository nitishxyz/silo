# Phase 8: scripting, the login server, smaller gaps

Goal: Silo is configured and automated in Lua, a machine can host several people, and the gaps the November
review found are closed. Designs: [`LUA.md`](LUA.md), [`LOGIN_SERVER.md`](LOGIN_SERVER.md). This file has the
decisions, the smaller gaps and the tasks.

## 1. Decisions

1. **Config lives on servers**, in `init.lua`. A client takes its keys from one home server (a Mac: This Mac) and
   actions from every connected host. No per-app config (`LUA.md` §2).
2. **Lua 5.4**, not Luau: scripts run as the user, so the sandbox bought nothing (`LUA.md` §3).
3. **Config handlers run in the server**, so reactions work with no app open.
4. **`action.run` needs `control` on the server**: the config can run commands as the server's user.
5. **The login server is a broker**: it admits a device to an account's own server and steps aside
   (`LOGIN_SERVER.md` §2). Identity is the device key.
6. **Deferred:** offering to rerun the last command after a crash restore; screen-reader support; PAM sessions;
   block plugins; a home-server setting for iPad and phone clients.

## 2. Smaller gaps

| Gap | Fix | Task |
|---|---|---|
| Share links lead to a 404 | `site/share/index.html`: explains the link, a button that opens it in Silo (`silo://share#…`), the install command; reads the ticket from the fragment, sends it nowhere | G1 |
| No share verb in the CLI | `silo share [SESSION] [--block B] [--rights view\|interact] [--ttl 1d] [--label L]` prints the link (and `--qr`); `silo share --list`, `--revoke ID` | P8-4 |
| Flaky tests | `stream::control::owner_name_follows_take_and_fallback`, `harness::a_streamed_tool_call_starts_with_its_name_before_any_input`, `daemon::a_daemon_opening_its_store_answers_starting_and_a_second_start_leaves_it`: find the race, fix the test or the code, 50 clean runs each | G2 |
| Linux CI skips otto and clippy | `ci.yml` linux job: add `-p silo-tools -p silo-lua -p silo-login` and every `otto-*` crate to the tests, and a clippy step over the same crates | G1 |
| Browser: no find in page | ⌘F in a browser block: a find bar (next, previous, count, Escape), CEF's find | G3 |
| Browser: no file chooser | `<input type=file>` opens the macOS open panel (multiple, accept filters) | G3 |
| Browser: permission prompts fall to CEF's defaults | Camera, microphone, location, notifications: a prompt in the pane (Allow once, Always for this site, Deny), remembered per origin in the profile | G3 |
| The app's relay cannot be set | Settings › Hosts: Relay (Default, Off, a URL); `relay` in `app.json`; applied to the app's endpoint at start | G3 |
| MCP elicitation is declined | `elicitation/create` becomes a question card in the agent block (the form's fields; accept, decline, cancel); without a UI it is declined as now | G4 |
| Docs behind the code | `GAPS.md` retired into `PLAN.md`; `PLAN.md` §8 crate layout; `OTTO.md` §0; README (Lua, login server); `HANDOFF.md` rewritten at the end | G1, final |

## 3. Tasks

Agents work in the same tree. A task owns its files; another task changes them only where this list says so,
with small targeted edits. Nobody runs tests until the final review (G2 excepted, in its own worktree). A
compile check of one's own crate is fine.

### Wave 0

**P8-1: protocol.** Alone on `silo-proto`; everything else builds on it.
- **Files:** `silo-proto/src/{config.rs (new), keys.rs (new), login.rs (new), control.rs, access.rs, api.rs,
  compat.rs, paths.rs, lib.rs}`, `silo-proto/protocol.json`, `silo-proto/tests/protocol.rs`;
  `silo-server/src/{scripting.rs (new, stub), login.rs (new, stub), conn.rs, access.rs, lib.rs}`; the arms other
  crates need to keep compiling (`silo-server/src/app_bridge.rs`, `silo-app/src/app_requests.rs`,
  `silo-cli`).
- **Work:** everything in `LUA.md` §7 and `LOGIN_SERVER.md` §10 that is protocol: types, methods, events,
  features, the need table (`Need::Login`), result schemas, examples, `protocol.json`. `silo_proto::keys` is the
  app's `Chord` (its pure parts) plus `Sequence` and `parse`. `conn.rs` dispatches `config.*`/`action.*` to
  `scripting::handle` and `login.*` to `login::handle`, both stubs answering `method_not_found`.
  `ClientRole::Login`.
- **Acceptance:** the need table stays exhaustive; `every_method_declares_access` and the protocol tests pass;
  `protocol.json` changes additively; the workspace compiles.

**P8-2: `silo-lua`.** `LUA.md` §3, §4, §9.
- **Files:** `crates/silo-lua/**` (new), the workspace `Cargo.toml` (its entry, mlua), `corgi.toml`
  (`[extra-inputs]` for any `.lua` it embeds).
- **Acceptance:** unit tests for loading (bindings, modes, unbinds, actions, problems, a syntax error),
  argument checking, derived events, timers, limits (a busy loop is stopped), `silo.exec`, the layout builder
  against a fake API; `check(path)` works without a server.

**G1: site, CI, docs.** `site/share/index.html`, `.github/workflows/{ci,pages}.yml`, `docs/{GAPS,PLAN,OTTO}.md`,
`README.md`.

**G2: flaky tests.** In a git worktree; the three tests of §2 and whatever code they show to be racy.

**G4: MCP elicitation.** `otto-mcp`, `otto-core`, `otto-proto`, `otto-server`, `silo-app/src/agent/**`.

### Wave 1

**P8-3: server scripting.** `LUA.md` §8.
- **Files:** `silo-server/src/{scripting.rs, tools.rs (a stand-in `Control` for a caller), lib.rs (`Config.scripting`,
  start and stop), block.rs or shell.rs (`SILO_SESSION`)}`, `silo-server/Cargo.toml`,
  `silo-server/tests/scripting.rs` (new).
- **Acceptance:** `config.get` after a load; a reload on a file change; a failed load keeps the last good config
  and reports the problem; `action.run` as the owner works and as a session-scoped token is `forbidden`; an
  action's calls carry the caller's rights; a config handler reacts to `block.cwd_changed` with no client
  subscribed; `config.changed` reaches a subscribed owner.

**P8-4: CLI.** `LUA.md` §5; `silo share` (§2).
- **Files:** `silo-cli/src/{main.rs, script.rs (new), config_cmd.rs (new), lib.rs}`, `silo-cli/Cargo.toml`,
  `silo-cli/tests/{lua.rs (new), verbs.rs}`. Wires `silo login-server` to `silo_cli::login_cmd::{LoginCommand,
  run}` (P8-6 writes that module).
- **Acceptance:** `silo do` tells a script, an action and a method apart; a script's return value, exit code,
  `--all`, `--for`, `--fail-fast` and arguments; `silo config check` on a good and a bad file; `silo actions`,
  `silo keymap`; `silo share` prints a link `silo grants` then lists.

**P8-5: app.** `LUA.md` §6.
- **Files:** `silo-app/src/{keymap.rs, keymap/**, commands.rs, palette.rs, workspace.rs, app_requests.rs,
  finder.rs, terminal_links.rs, titlebar/**, settings_panel/keyboard_page.rs, hosts_state.rs}`, new
  `silo-app/src/{config.rs, modes.rs}`.
- **Acceptance:** unit tests for the merged keymap (defaults, `keybindings.json`, config binds and unbinds,
  sequences, modes, unknown targets as problems), mode state (enter, once, exit, exclusive), target resolution
  (command, method with focus ids filled, action on the focused host else home), the args prompt parser.

**P8-6: login server.** `LOGIN_SERVER.md`.
- **Files:** `crates/silo-login/**` (new), `silo-transport/src/{login.rs (new), net.rs, lib.rs}`,
  `silo-server/src/{login.rs, conn.rs (admitting `role: login`), auth.rs, grants.rs, lib.rs (the uid 0 peer)}`,
  `silo-client/src/{login.rs (new), hosts.rs, lib.rs}`, `silo-cli/src/{login_cmd.rs (new), link.rs, lib.rs}`,
  `silo-app/src/add_host.rs`, `packaging/linux/**`, `silo-server/tests/login.rs`, `silo-login/tests/**`.
- **Acceptance:** `LOGIN_SERVER.md` §11.

**G3: browser and relay.** `silo-web/src/**`, `silo-app/src/{browser_pane.rs, browser_pane/**, remote.rs,
settings_table.rs, settings_panel.rs (the relay row), hosts.rs (`app.json`)}`; one command for find, if none
fits, in `commands.rs`.

### Final

Build everything (`corgi` for the crates it can, `cargo` for the app), clippy, every test, then by hand in a
throwaway home: an `init.lua` with a binding, a mode, an action, a handler; `silo config check`, `silo do`, a
script with events; the login flow unprivileged. Then `HANDOFF.md`.

## 4. Status (2026-11-15)

Every task above is built and released in 0.1.34. On macOS: fmt and clippy clean, every test passes (four
`silo-server` tests need a plain zsh: `HANDOFF.md`). The Lua side and the app were run by hand and through the UI
driver in throwaway homes, and the login flow unprivileged; the login server has not run as root, and the MCP
elicitation card has not been seen with a real server. What differs from the designs is under "As built" in
`LUA.md` and `LOGIN_SERVER.md`; the open ends are in `HANDOFF.md`.

An action's arguments in the CLI and ⌘P (a bare value, `--name VALUE`, `name=VALUE`) were added after the
design: `LUA.md` §5 and §6.
