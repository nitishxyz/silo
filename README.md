<p align="center">
  <img src="assets/brand/silo-app-icon.svg" width="96" alt="Silo">
</p>

<h1 align="center">Silo</h1>

<p align="center"><b>Your workspace, always running, anywhere.</b><br>
Terminals, browsers, AI agents and files in one native Mac app, kept alive by a server on any machine you own.</p>

<p align="center">
  <a href="https://github.com/nitishxyz/silo/releases/latest">Download for macOS</a> ·
  <a href="#install">Install</a> ·
  <a href="#the-cli">CLI</a> ·
  <a href="#building-from-source">Build</a>
</p>

<!-- Demo: record it, then `scripts/demo-media.sh recording.mov` writes docs/media/demo.gif.
<p align="center">
  <img src="docs/media/demo.gif" width="880" alt="Silo: tabs of terminals, a browser and an agent, split and merged">
</p>
-->

---

## What it is

Silo is one window for everything you work in: terminals, a browser, AI coding agents and your files, side by side in tabs. You can split a tab, rearrange it, or drag one tab into another.

| In a tab | What you get |
| --- | --- |
| **Terminal** | A real, fast terminal on [libghostty](https://github.com/ghostty-org/ghostty), with full scrollback, images and shell integration. |
| **Browser** | A full Chromium browser with tabs, history, downloads and DevTools. Its toolbar takes on the page's colour. |
| **Agent** | *otto*, Silo's built-in coding agent. It works with Claude, GPT, Gemini or local models, and has tools, MCP and approvals. |
| **Files** | A folder as a tree and an editor, or as a Finder-like browser, updated live as files change. |
| **Changes** | A review of diffs: your working tree, what's staged, or what an agent changed. |

What makes it different: all of this runs on a **Silo server** on a machine you own (your Mac, a Linux box or a cloud VM), not inside the window. Close the app, restart your Mac or switch to another one, and your shells, pages and agent sessions keep running. When you come back, they're exactly where you left them.

## Highlights

- **Always running.** The server keeps every program alive, snapshots terminals and restores them after a crash or an upgrade. Upgrades happen live, with sessions kept.
- **Any machine.** Run the server on your Mac, a Linux box or a VM. Pair over an end-to-end encrypted peer-to-peer connection ([iroh](https://iroh.computer)), so there's no port forwarding, VPN or SSH session to keep alive. `silo remote install` sets up a machine over SSH in one step.
- **Native and fast.** Built in Rust on [GPUI](https://www.gpui.rs), with GPU rendering, spring motion and real backdrop blur. Typing latency is measured and tested.
- **Made for agents.** Agents (otto, Claude Code, Codex, or anything that speaks MCP) can drive terminals and browsers through `silo mcp`, under scoped, revocable grants. You see what needs you through tab marks, notifications and the Dock badge.
- **Keyboard first.** ⌘P finds anything: commands, settings, tabs, files and pages. ⌘T opens a tab, ⌘D splits, ⌘K switches sessions. Drag a tab onto another to merge them.
- **Calm.** The window and padding take on the colour of the app in focus. Interface sounds are soft and subtle. Notifications only arrive for what you're not looking at.
- **Share.** Hand a session or a single pane to another Silo with a link, read-only or interactive.

## Install

**macOS (Apple silicon):** download the latest `Silo-x.y.z.dmg` from [Releases](https://github.com/nitishxyz/silo/releases/latest) and drag Silo to Applications. The app includes its server and the `silo` CLI, and updates itself.

**A server on Linux or macOS** (x86_64, arm64):

```sh
curl -fsSL https://nitishxyz.github.io/silo/install.sh | sh
```

This installs `silo` to `~/.local/bin` and starts the server as a user service (systemd or launchd). To add the machine to your Mac:

```sh
silo remote install user@host     # from the Mac, over SSH: install, start and pair in one go
# or, on the machine itself:
silo pair                         # prints a ticket; paste it into Silo › Add Host
```

## The CLI

`silo` runs the server and scripts it. Every command takes `--host <name>` to act on a paired machine.

```sh
silo new -s work           # a session, attached in this terminal (detach: Ctrl-\)
silo ls                    # sessions
silo run work -- cargo test   # run in a session's shell, print the output, exit with its code
silo capture work          # a terminal's text
silo browser navigate -t work example.com
silo report working        # tell Silo what your program is doing (tab marks, notifications)
silo mcp                   # Silo's tools for any MCP agent
silo update                # the latest release; a running server upgrades live
```

`silo api list` and `silo do <method> --help` reach the whole control API: everything the app does is a method you can call.

## Building from source

You need a recent stable Rust and Xcode's command line tools. Zig and CEF are fetched for you.

```sh
scripts/build-ghostty.sh              # libghostty-vt, pinned in GHOSTTY_REV
cargo build                           # the app, the server and the CLI
scripts/dev-bundle.sh                 # wrap them as target/debug/SiloDev.app
open target/debug/SiloDev.app
```

`SiloDev` keeps its own data (`~/Library/Application Support/silo-dev`) apart from an installed Silo. Run `scripts/check.sh` before sending a change (fmt, clippy, and the tests of what changed; `--all` runs everything). Linux builds only the CLI and server; see [`packaging/linux`](packaging/linux/README.md).

<details>
<summary>How it's put together</summary>

| Crate | Role |
| --- | --- |
| `silo-app` | The macOS app (GPUI) |
| `silo-server` | Sessions, blocks, PTYs, snapshots, access rules and the control API |
| `silo-cli` | The `silo` binary: the server, the CLI, MCP |
| `silo-proto` | Wire types, the method list and the compatibility rules (`protocol.json` pins every schema) |
| `silo-client`, `silo-transport` | Connections over a local socket or iroh, with reconnects |
| `silo-vt`, `silo-pty`, `silo-render` | libghostty-vt bindings, PTYs and terminal rendering |
| `silo-web` | CEF browser blocks |
| `otto-*` | The agent: providers, the turn loop, tools, MCP client, store and daemon |

Design notes are in [`docs/`](docs): [`PLAN.md`](docs/PLAN.md) for the architecture, [`BLOCKS.md`](docs/BLOCKS.md) for how block kinds plug in, and [`OTTO.md`](docs/OTTO.md) for the agent.
</details>

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in Silo by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
