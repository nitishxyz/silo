# Lua: config, actions and scripts

Status: built (Phase 8); the "As built" notes say where it differs from the design. Builds on the one API (`docs/PHASE7.md` §4): every
call a script makes is a control method, with the caller's rights.

## 1. What it is for

1. **Custom commands.** A named Lua function shows in ⌘P, binds to a key and runs from the CLI
   (`silo do dev_layout app.log`).
2. **Project workspaces.** One script builds a tab of terminals, a browser, an agent, a Files and a Changes block.
   It can live in the repo.
3. **Reactions.** Label a tab by its folder, zoom the pane whose tests failed, open Changes when an agent's turn
   ends. Handlers in the config run in the server, so they work with no app open.
4. **Prefix keys and modes** (tmux's `ctrl-b c`, a resize mode on the arrow keys).
5. **Driving Silo from other programs.** An editor mapping calls `silo do <action>`; the action opens a block,
   moves focus, types into a terminal. §10 has neovim both ways.
6. **Agents.** An agent writes one script instead of many tool calls; an action is a tool it can call.
7. **Plugins.** A plugin is a folder of Lua loaded with `require`.

## 2. Where config lives

- **All config is on servers.** `init.lua` is read by the server of the machine it is on. Nothing is configured
  per app.
- **A client has one home server.** Its keys come only from there. A Mac's home server is This Mac. An iPad or
  phone picks one (its first paired host until it has a setting).
- **Actions come from every connected host.** ⌘P lists the focused session's host's actions and the home
  server's. A key bound to an action runs it on the focused session's host when that host defines it, else on the
  home server.
- **Other hosts never set a client's keys** and never drive it (`PHASE7.md` §4.2 keeps holding).
- **State stays per device:** paired hosts and the device key, window positions, font, theme and density.
  `keybindings.json` (Settings › Keyboard writes it) stays; `init.lua` is applied on top of it.

**File:** `$SILO_CONFIG`, else `$XDG_CONFIG_HOME/silo/init.lua` (default `~/.config/silo/init.lua`; the dev
channel uses `silo-dev`) when the server's home is the channel's default, else `<home>/config/init.lua` (tests, UI
runs). `silo_proto::paths::config_file(home)`. `require("keys")` loads `keys.lua` beside it.

The server loads it at start, on `config.reload`, and when the file or a required file changes (watched,
debounced 500 ms). A load that fails keeps the last good config and reports why.

## 3. Runtime

- **Lua 5.4**, vendored (mlua, features `lua54 vendored async serialize`). Not Luau (`PHASE7.md` had it): the
  config and scripts run as the user anyway (`silo.exec`), so a sandbox buys nothing, plain Lua is what people
  know, and it is a C build with no C++.
- Standard library with `io` and `os`. Removed: `io.popen`, `os.execute` (use `silo.exec`), `os.exit` (use
  `silo.stop`), `package.loadlib` and C modules (`package.cpath = ""`).
- Everything Silo adds is in the global `silo`.
- **One VM per loaded config and one per script**, each on its own thread. Calls into Silo are async underneath,
  so an action waiting on a call or a command does not block other actions, handlers or the server.
- **Limits:** an action 30 s (`timeout` up to 600), a handler or timer callback 10 s, enforced at calls into Silo
  and by an instruction hook, so `while true do end` is stopped too. 256 MiB per VM. 1,024 timers. `silo.invoke`
  16 deep. 1,024 queued events per VM (the oldest is dropped, logged once).

Two places run Lua:

| | Config | Script |
|---|---|---|
| Started by | the server, from `init.lua` | `silo do file.lua`, `silo do -e CODE`, `silo do -` |
| Runs on | the server's machine | the machine the CLI is on |
| Lives | until the next load | until its last line, or while it has handlers or timers |
| Calls Silo as | the caller of the action; the owner for handlers and timers | the CLI's connection |

## 4. The `silo` API

A call that fails returns `nil, message, code` and never raises; `error(msg, 0)` fails an action or script.

### 4.1 Keys (config, at load)

```lua
silo.bind("cmd-e", "SplitRight")                      -- an app command, by id (Settings › Keyboard lists them)
silo.bind("cmd-shift-l", "dev_layout", { file = "app.log" })   -- an action
silo.bind("ctrl-b c", "window.create")                -- a control method; ids come from the focus
silo.bind("ctrl-b %", function(ctx) silo.block.split{ axis = "horizontal" } end)
silo.unbind("cmd-k")                                  -- also removes the app's default
silo.unbind("cmd-k", "SwitchSession")                 -- only that binding

silo.mode("resize", { exclusive = true })
silo.bind("cmd-r", "app.mode_enter", { name = "resize" })
silo.bind("resize/left", "MovePaneEdgeLeft")
silo.bind("resize/enter", "app.mode_exit")
```

- **Keys:** a chord is `ctrl-alt-shift-cmd-key`, as in `keybindings.json`. A sequence is chords separated by
  spaces. `mode/keys` binds in a mode; a mode name is `[a-z][a-z0-9_]*`. `silo_proto::keys` parses them; the app's
  `Chord` becomes that type.
- **Target**, decided by its spelling:
  - a dot: a control method. `app.*` runs in the client. Anything else is called on the focused session's host,
    with `session_id`, `window_id` and `block_id` filled from the focus when the method takes them and `args`
    leaves them out.
  - an uppercase first letter: an app command id (`Command::id`). No args.
  - otherwise: an action name.
  - a function: an action named `bind_<n>`, hidden from ⌘P.
- **Modes:** `app.mode_enter {name, once?}`, `app.mode_exit {all?}`. Escape leaves the current mode unless the
  mode binds it. Only the most recently entered mode's bindings apply. In an `exclusive` mode, keys it does not
  bind do nothing; otherwise they do what they do outside it. `silo.unbind("resize/left")` blocks the key in the
  mode.
- In a script these do nothing and warn.

### 4.2 Actions

```lua
silo.action{
  name = "dev_layout", title = "Open Dev Layout",
  description = "Editor, server and a browser", category = "Project", keywords = "start work",
  args = { file = "string", ratio = "number?" },
  run = function(ctx, args)
    local w, err = silo.open{ layout = silo.layout.row(args.ratio or 0.6,
      silo.layout.block{ command = { "nvim", args.file } },
      silo.layout.column(0.5,
        silo.layout.block{ command = { "npm", "run", "dev" } },
        silo.layout.block{ kind = "browser", url = "http://localhost:3000" })) }
    if not w then error(err, 0) end
    return { window_id = w.window_id }
  end,
}
```

- `name`: `[a-z][a-z0-9_]*`. `title` and `run` are required. A later definition of a name replaces the earlier
  and warns.
- `args`: `string`, `number`, `integer`, `boolean`, `object`, `array`, `any`, `?` for optional; or a JSON Schema
  (a table with `type`). Checked before `run`. Without `args`, anything is accepted.
- `ctx`: `session_id`, `window_id`, `block_id`, `client_id` (absent when unknown), `origin` (`key`, `palette`,
  `cli`, `api`, `script`, `event`, `timer`) and `host` (the server's name).
- What `run` returns is the result, as JSON. Nothing, `nil` or `true`: no result.
- `silo.invoke(name, args)` runs another action with the same `ctx`. In a script, a name the script does not
  define is run by the server.
- **Reserved names** the app calls instead of its default when the focused session's host (else the home server)
  defines them: `open_file {path, line?, column?}` (a path clicked in a terminal, a file picked in ⌘P) and
  `open_url {url}` (a link clicked in a terminal). An action that returns `false` hands back to the default.

### 4.3 Calling Silo (not at config load)

```lua
local view, err = silo.session.view{}                 -- silo.<group>.<name>(params) is silo.call("<group>.<name>", params)
silo.call("terminal.send", { block_id = id, text = "make test\n" })
silo.app.palette{}                                    -- an app.* method, to the client that ran the action
```

- `silo.call(method, params)` sends exactly `params`.
- `silo.<group>.<name>(params)` fills `session_id`, `window_id` and `block_id` from `ctx` (in a script: where it
  was run, or `-s`/`-b`) when the method takes them and `params` leaves them out. Groups and params come from the
  API description (`silo api list`, `silo api describe <method>`).
- `silo.app.<name>` adds `client_id = ctx.client_id`. It needs an app on the server's machine (`no_app`
  otherwise), as every `app.*` call does.
- At config load there is no caller: a call fails with a warning and the load goes on.

### 4.4 Layouts

- `silo.layout.block{ kind?, command?, cwd?, url? }`: `kind` is `terminal` (default), `browser`, `agent`, `files`
  or `changes`; `command` a list; `cwd` also the folder of a files or changes block.
- `silo.layout.row(ratio, a, b)` puts `a` left of `b`; `silo.layout.column(ratio, a, b)` puts `a` above `b`.
  `ratio` is `a`'s share.
- `silo.open{ layout, session_id?, beside?, name? }` builds it: a new tab of the session, or beside block
  `beside`. Returns `{ window_id, block_ids }` (blocks in layout order), or `nil, message`. It is `window.create`,
  `block.split` and `layout.ratio` calls; a failure part-way closes what it made.

### 4.5 Events, waiting and timers

```lua
silo.on("block.cwd_changed", function(ctx, ev)
  silo.window.rename{ window_id = ctx.window_id, name = ev.cwd:match("([^/]+)/?$") }
end)
silo.on("block.command_finished", function(ctx, ev)
  if ev.record.exit ~= 0 then silo.block.zoom{ block_id = ctx.block_id, zoomed = true } end
end)
local ev = silo.wait("block.exited", { block_id = id }, 600)   -- nil, "timeout"
local t = silo.every(5, export); t:cancel()
```

- `silo.on(event, fn(ctx, ev))`: registered while the file first runs. In the config, handlers hear every session
  and run in the server as the owner. In a script they hear the script's session, or every session with `--all`.
  One event at a time per VM; an error is reported and the handler stays.
- `silo.wait(event[, filter][, timeout])`: `filter` is a table of fields to equal or a function `(ev, ctx)`.
  `silo.sleep(seconds)`.
- `silo.after(seconds, fn)`, `silo.every(seconds, fn)`: a timer with `:cancel()` and `:active()`. Missed ticks
  are dropped.
- `silo.stop([code])` ends a script.
- **Events.** Every server event by its name (`block.bell`, `block.status`, `block.attention`,
  `block.command_finished`, `files.changed`, `changes.changed`, …; `silo events` prints them), plus events
  worked out from `sessions.changed`, `session.changed` and `block.changed`, the same in the server and the CLI:

  | Event | `ev` |
  |---|---|
  | `session.created`, `session.closed` | `session_id`, `name` |
  | `window.created`, `window.closed` | `window_id` |
  | `block.created`, `block.closed` | `block_id`, `kind` |
  | `block.exited` | `exit_code` |
  | `block.cwd_changed` | `cwd` |
  | `block.title_changed` | `title` |

  `ctx` has the event's `session_id` and, for a block event, `block_id` and `window_id`. A name nothing sends is
  not an error.

### 4.6 The rest

- `silo.exec(argv[, opts])` runs a command where the VM runs and waits: `opts` are `cwd`, `env`, `stdin`,
  `timeout`, `detach`; the result has `ok`, `code`, `signal`, `stdout`, `stderr`, `timed_out`. No shell reads
  `argv`. A command is stopped when the action, handler or script that started it ends, unless `detach`.
- `silo.json.encode(value[, {indent}])`, `silo.json.decode(text)`; `silo.log([level,] message)`.
- Script values: `silo.args`, `silo.session_id`, `silo.block_id`. Everywhere: `silo.version`, `silo.host`,
  `silo.config_dir`.

## 5. CLI

```sh
silo do dev_layout app.log             # an action (a name without a dot); a bare value is its one required arg
silo do dev_layout --file app.log --ratio 0.4   # or by name: --name VALUE, name=VALUE
silo do ./layout.lua name=web          # a script: ends in .lua, has a slash, or is `-`
silo do -e 'return silo.session.list{}'
silo do window.step --delta 1          # a method, as before
silo config path | check [FILE] | reload | show | init
silo actions                           # the server's actions
silo keymap                            # the config's bindings, modes and problems
```

- Script flags: `-s/--session`, `-b/--block`, `--all`, `--for 10m`, `--fail-fast`, `--args JSON|@file`. A
  script's arguments are `key=value`; `n=2` is a number, `on=true` a boolean. The returned value is printed as
  JSON. Exit: 0, 1 (an error), or `silo.stop`'s code.
- **An action's arguments** are read with what it declares:
  - `--name VALUE` or `--name=VALUE` (`--dry-run` is `dry_run`; a boolean's flag alone is `true`), or `name=VALUE`.
  - A bare value is the action's only required argument, or its only argument: `silo do name_tab work`. Several
    bare words are one string (`silo do name_tab my new tab`).
  - With two or more required arguments a bare value is refused, naming the flags. A flag the action does not
    declare is refused. An argument named like one of `silo do`'s own flags (`session`, `block`, `json`, …) is
    written `name=VALUE`.
- `silo config check` loads a file in the CLI without a server and prints what it declares and its problems
  (exit 1 on an error). `silo config init` writes a commented starter file if there is none.
- `silo config reload` asks the server; it prints the new state.
- Blocks get `SILO_SESSION` beside `SILO_BLOCK`.

**As built** (`silo-cli/src/{script.rs, config_cmd.rs, share.rs}`):

- `silo do` takes the first word as the target: `-`, `*.lua` or anything with a `/` is a script, then a dot
  makes a method, then `[a-z][a-z0-9_]*` an action; anything else is an error. Flags may come first
  (`silo do -s work x.lua`, `silo do -e CODE`); a method must come first. `ping`, the one method without a dot,
  reads as an action (the error names `silo api call ping`). `--` ends the flags.
- Values: `n=2` number (JSON numbers only: `007` stays a string), `true`/`false`, `null`, else a string. An
  action's arguments it declares as `string` stay strings (`name=42`). `--args JSON|@file` must be an object
  and wins per key.
- Where it runs: `-s` (id or name), `-b` (id). Else, only against the local server (not with `--host`):
  `$SILO_BLOCK` unless `-s` is given, and for scripts `$SILO_SESSION`, else the block's session (`block.view`).
  A stale `$SILO_BLOCK` is dropped. Actions get `block_id` from `-b`/`$SILO_BLOCK`, `session_id` only from `-s`.
- Scripts: `silo.config_dir` (and `require`) is the script file's folder, else the current directory. A `#!`
  first line is allowed. Output: the returned value as JSON (compact when piped, indented on a tty or with
  `--json`), `silo.log` and handler failures on stderr, `Error: …` (no traceback) and exit 1, or
  `silo.stop`'s code; a script still running once its top level returns prints `listening: N handlers;
  Ctrl-C to stop`; Ctrl-C exits 130, `--for` 0. `--all`, `--for` and `--fail-fast` are refused for actions.
  The CLI wraps the script on its first line to count `silo.on` calls and see the top level return
  (`silo-lua` does not say); line numbers are kept, but an unclosed construct is reported near `end`.
- Actions: `silo do NAME --help` prints its title, args and source. An error prints `Error: …`, exit 1.
- `silo do FILE.lua --action NAME k=v…` loads the file in the CLI as a config (`Mode::Config`; its keys,
  handlers and timers do nothing) and runs that action with the CLI's connection.
- `silo config check` prints `path`, `exists`, `declares: N bindings, N removals, N modes, N actions,
  N handlers, N timers`, then `warning|error SOURCE: message`; `--json` is a `ConfigState`. `show` and
  `reload` print the same plus the revision. `init` refuses an existing file.
- `silo actions [--all] [--json]`: NAME, TITLE, CATEGORY, ARGS (`file:string ratio:number?`), SOURCE; `--all`
  adds hidden ones. `silo keymap [--json]`: KEYS (`mode/keys`), TARGET, ARGS, SOURCE; then removals, modes and
  problems.
- `silo share [SESSION] [--block B] [--rights view|interact] [--ttl 1d] [--label L] [--qr]` prints the
  `https://…/share/#silos1…` link (default session: `$SILO_SESSION`, else `$SILO_BLOCK`'s; no default
  otherwise); `--list` lists link grants, `--revoke ID` revokes a link (an id prefix; never another kind of
  grant). Approvals go through the usual prompt. `--host` works for `do`, `config reload|show`, `actions`,
  `keymap` and `share`.

## 6. App

- **Keymap:** defaults, then `keybindings.json`, then the home server's `init.lua` (`config.get`, again on
  `config.changed`). Sequences and modes are gpui key bindings (multi-keystroke, key contexts). Hints in menus,
  ⌘P and tooltips show the merged result. Settings › Keyboard says how many shortcuts come from `init.lua`, with
  Open and Reload, and lists the config's problems.
- **⌘P:** the actions of the focused session's host and the home server, grouped by `category`. An action with
  required args asks for them. One required arg is typed alone (the prompt shows its name; `my new tab`), with
  optional ones after it as `key=value`; two or more are `file=app.log ratio=0.4`.
- **Running:** `action.run` on the host, with the focus as `session_id`, `window_id`, `block_id`. An error is a
  toast.
- **Mode:** shown in the title bar while one is active.

**As built** (`silo-app`: `config.rs`, `keymap.rs`, `modes.rs`):

- Configs are kept per host (`config::get`), fetched by `HostsState` on connect and `config.changed`. No feature,
  `method_not_found` or a refusal (a share link) is no config; a dropped connection keeps the last one. The home
  server is the host with `info.local`.
- Merge: the defaults and `keybindings.json` stay what Settings edits; `init.lua`'s unbinds, then bindings, go on
  top. A chord of theirs that a config binding uses, or starts (`ctrl-b` under `ctrl-b c`), is taken from its
  command and noted as a problem; two config bindings of the same keys: the last wins, noted.
- A config binding to a command (outside modes) is that command's gpui action, bound first, so menus and hints show
  it. Every other config binding is one action, `RunBinding { index }`, into the merged list.
- Modes are gpui key contexts: the workspace context names each declared mode not on top `silo_off_<mode>`, and a
  mode's bindings are `!silo_off_<mode>` (a negation matches at the deepest context, so they outrank the rest, and
  come last to win ties). An exclusive mode adds `silo_exclusive`; when the config declares one, the bindings
  outside modes are `!silo_exclusive`, and a capture key listener on the workspace swallows the keys no binding took.
  A blocked key is a `RunBinding` that does nothing. `once` leaves after the next binding of that mode runs.
  Escape and exclusive leave modals (⌘P, Settings, sheets) alone.
- A `file://` link clicked in a terminal is the path `open_file` gets; plain paths in terminal text are not links
  yet. ⌘P's file picks pass `path` (absolute) and `line`; `column` is never sent.
- ⌘P's `>` (commands) chip includes actions. A click has no `origin`; ⌘P's runs are `palette`.

## 7. Protocol (additive; feature `config`)

`silo-proto/src/config.rs`:

```rust
pub struct ConfigState {
    pub revision: u64,            // bumps on every applied load
    pub path: String,
    pub exists: bool,
    pub loaded_at: u64,           // unix ms; 0: never
    pub failed: bool,             // the last load failed; this is the previous good state plus its problems
    pub bindings: Vec<KeyBinding>,
    pub unbinds: Vec<KeyUnbind>,
    pub modes: Vec<KeyMode>,
    pub actions: Vec<ActionInfo>,
    pub handlers: Vec<HandlerInfo>,
    pub timers: u32,
    pub problems: Vec<ConfigProblem>,
}
pub struct KeyBinding { pub keys: String, pub mode: Option<String>, pub target: BindTarget, pub args: Value, pub source: String }
#[serde(tag = "type", rename_all = "lowercase")]
pub enum BindTarget { Command { id: String }, Method { name: String }, Action { name: String }, #[serde(other)] Unknown }
pub struct KeyUnbind { pub keys: String, pub mode: Option<String>, pub target: Option<String>, pub source: String }
pub struct KeyMode { pub name: String, pub exclusive: bool, pub source: String }
pub struct ActionInfo {
    pub name: String, pub title: String, pub description: String, pub category: String,
    pub keywords: Vec<String>, pub args: Value /* JSON Schema, {} for any */, pub repeats: bool,
    pub hidden: bool, pub source: String,
}
pub struct HandlerInfo { pub event: String, pub source: String }
pub struct ConfigProblem { pub level: ProblemLevel /* warning | error */, pub source: String, pub message: String }
pub struct ActionRunParams {
    pub name: String, pub args: Value,
    pub session_id: Option<SessionId>, pub window_id: Option<WindowId>, pub block_id: Option<BlockId>,
    pub client_id: Option<ClientId>, pub origin: Option<ActionOrigin>,
}
pub enum ActionOrigin { Key, Palette, Cli, Api, Script, Event, Timer, #[serde(other)] Unknown }
pub struct ActionResult { pub result: Value }
pub struct ActionList { pub actions: Vec<ActionInfo> }
```

- **Methods:** `config.get {} → ConfigState`, `config.reload {} → ConfigState`, `action.list {} → ActionList`,
  `action.run ActionRunParams → ActionResult`. All need `control` on the server: the config can run commands as
  the server's user, so a share link or a session-scoped token gets none of them.
- **App methods:** `app.mode_enter {name, once?, client_id?}`, `app.mode_exit {all?, client_id?}`.
- **Event:** `config.changed {revision}`, to clients with `control` on the server.
- `silo_proto::keys`: `Chord`, `Sequence`, `parse(text) -> Result<Keys, String>` (`Keys {mode, sequence}`).
- `silo_proto::paths::{config_dir, config_file}`.

## 8. Server

- `silo-server/src/scripting.rs`: owns the config VM (`silo-lua`), its watcher and `ConfigState`.
- **An action** runs on a stand-in connection as its caller (`tools.rs` `StandIns`), so every call inside has
  the caller's rights, pane and approvals.
- **Handlers and timers** run on a stand-in as the owner, labelled `init.lua`.
- A reload builds a new VM, swaps it in when the load succeeds, and stops the old one (its timers and running
  commands) once the actions running in it finish or 30 s pass.
- `SILO_CONFIG=off` (or `Config.scripting: false`, what tests use) loads nothing.
- A hot upgrade loads the config afresh in the new server; nothing of the VM is handed over.

As built:

- `Server.scripting` (`scripting::Scripting`); `scripting::run` is spawned once the socket listens, so a config
  never delays or fails a start. A missing file is an empty config (`exists: false`) with no VM thread.
- The watcher watches folders, not files (editors replace by rename): the config's folder and the folder of every
  file the VM or the last failed load read. A folder that does not exist yet is looked for every 3 s, not watched
  through an ancestor (that could be the home folder); a server without a config folder has no watcher.
- `revision` bumps on every load that changes the state, a failed one too (its `failed` and `problems` changed),
  and `config.changed` goes out for both; a missing file that stays missing changes nothing.
- On a swap the old VM's handlers and timers lose their owner connection at once (they would otherwise run beside
  the new VM's); its running actions finish on their callers' connections, then it is stopped (30 s at most).
- One owner stand-in connection per VM (`init.lua`, passive, so it never keeps an idle server up). Callers'
  stand-ins are `tools::StandIns::caller`, kept per client until it disconnects.
- `action.run` defaults: `client_id` the caller, `origin` `api`, `block_id` the caller's pane, and `session_id`,
  `window_id` from that block (the window only when `session_id` is absent or the block's).
- `SILO_CONFIG=off` or `Config.scripting: false`: `config.get`/`config.reload` answer an empty `ConfigState`,
  `action.list` none, `action.run` `not_found`.
- `SILO_SESSION` is the session a block started in; a block moved later keeps it.

## 9. `silo-lua`

A new crate (`silo-proto`, `silo-client`, mlua, tokio): the VM, the `silo` table, the derived events, `silo.exec`
and the layout builder. It knows nothing of the server: it is given a `Control` and its `Events`, so the server
(stand-ins) and the CLI use the same code.

```rust
pub enum Mode { Config, Script }
pub struct Options { pub mode: Mode, pub dir: PathBuf, pub source_name: String, pub host: String, pub args: Value, pub all_sessions: bool }
pub struct Loaded { pub bindings, unbinds, modes, actions, handlers, timers, problems, failed: bool }  // the parts of ConfigState
pub struct Vm { /* a handle to the VM's thread */ }
impl Vm {
    pub fn load(source: Source, options: Options) -> (Vm, Loaded);               // runs the file top to bottom
    pub async fn run_action(&self, name: &str, call: Invocation) -> Result<Value, RpcError>;
    pub fn attach(&self, api: Api, events: Events);                              // handlers and timers start hearing events
    pub async fn finished(&self) -> i32;                                         // a script's exit code
    pub fn stop(&self);
}
pub struct Invocation { pub ctx: Ctx, pub args: Value, pub api: Api }
pub struct Api { /* a Control, plus where ids default from */ }
pub fn check(path: &Path) -> Loaded;                                            // `silo config check`
```

## 10. Examples

**Neovim → Silo.** A mapping opens the file's preview beside the editor:

```lua
-- nvim
vim.keymap.set("n", "<leader>p", function()
  vim.system({ "silo", "do", "preview", "path=" .. vim.fn.expand("%:p") })
end)
-- init.lua (Silo)
silo.action{ name = "preview", title = "Preview File", args = { path = "string" },
  run = function(ctx, a)
    silo.block.split{ axis = "horizontal", kind = "browser", url = "file://" .. a.path }
  end }
```

**Silo → neovim.** A path clicked in a terminal opens in the neovim of that tab, or in a new pane:

```lua
silo.action{ name = "open_file", title = "Open in Neovim", args = { path = "string", line = "integer?" },
  run = function(ctx, a)
    local view = silo.session.view{}
    for _, b in ipairs(view.blocks) do
      if b.title:find("nvim") then
        silo.terminal.send{ block_id = b.id, text = "\27:e +" .. (a.line or 1) .. " " .. a.path .. "\r" }
        return silo.block.focus{ block_id = b.id }
      end
    end
    silo.block.split{ axis = "horizontal", spawn = { command = { "nvim", "+" .. (a.line or 1), a.path } } }
  end }
```

**Tabs named by folder; failed tests zoom:** §4.5. **A project layout:** §4.2, run as
`silo do ./dev.lua` from the repo or bound to a key.

## 11. Later

- A setting for a client's home server (iPad, phone), and `app.*` calls reaching a remote client whose home
  server this is.
- `silo.exec.start` with streamed output; `silo.host{}` (declaring hosts); syncing `init.lua` between a user's
  hosts.
- Block plugins (a block kind provided by a program).
- Actions as tools for agents (`tools.provide` from the config).
