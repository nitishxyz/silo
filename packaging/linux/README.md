# silo on Linux

The `silo` binary (crate `silo-cli`) runs the server as a systemd user
service. The GUI crates are macOS-only and not built here.

## Build

```sh
bash scripts/build-ghostty.sh          # zig 0.16.0 is fetched into target/tools
cargo build -p silo-cli --release      # target/release/silo
```

To cross-build from another host, pass the Rust triple to the script
(`scripts/build-ghostty.sh x86_64-unknown-linux-gnu`); `silo-vt`'s build
script uses `target/ghostty/<rev>/runtime-<triple>` and refuses a runtime
built for a different target. A C cross compiler for `silo-vt`'s adapter
(e.g. a `zig cc -target x86_64-linux-gnu` wrapper in
`CC_x86_64_unknown_linux_gnu`) and a linker are also needed.

On machines with little memory (8 GB or less), limit parallel rustc jobs
(`CARGO_BUILD_JOBS=2`): full-width builds have been OOM-killed. CI
(`.github/workflows/linux.yml`) does the same and uploads the release
binary as the `silo-linux-x86_64` artifact.

## Install

```sh
install -Dm755 silo ~/.local/bin/silo
~/.local/bin/silo up
```

`silo up` writes the systemd user unit for the binary it was run as, enables
and starts it, and enables lingering so the server keeps running without a
login session (when `loginctl enable-linger` is not allowed it prints the
`sudo` command). A server already running outside the unit is taken over
live. `silo status` shows the server and whether systemd manages it. Without
a reachable systemd user manager (or with `SILO_SERVICE_MANAGER=none`, e.g.
in a container), `silo up` starts a plain background server that does not
survive a reboot.

State lives in `~/.local/state/silo` (the Linux default of `SILO_HOME`; the
unit pins the value `silo up` saw). If your shell sets `SILO_HOME` or
`XDG_STATE_HOME` to something else, CLI clients will look for a different
server: keep them in agreement. Logs go to the journal: `journalctl --user -u silo`.

## Pairing

From a Mac running Silo, `silo remote install <ssh-target>` installs silo on
the machine, starts it and adds it as a host (`--binary <path>` uploads a
local build instead of downloading the release). `silo join <ticket>` offers
this machine to the one whose join ticket it is. Running `silo up` over SSH
from a Silo terminal offers the machine inline (`--no-offer` skips that).

Otherwise run `silo pair` on the server to print a pairing ticket and enter
it on the client device (`silo hosts add <ticket>` for the CLI).

By default the server's remote endpoint uses n0 relays and a random UDP
port. Without relays (`--relay disabled`, or a network that blocks them) and
without mDNS, paired devices reach the server only at its stored direct
address, which changes with the port on every restart. Pin the port with a
drop-in (`systemctl --user edit silo`; `silo up` leaves drop-ins alone),
passing it to both the server and the upgrade successor, and allow it
through the firewall:

```ini
[Service]
ExecStart=
ExecStart=%h/.local/bin/silo server --adopt --relay disabled --port 7848
ExecReload=
ExecReload=%h/.local/bin/silo server --takeover --daemon --idle-exit 0 --relay disabled --port 7848
```

`silo id` and `silo pair` report when remote access is unavailable (e.g. the
port is already in use).

## Stop

`silo down` stops the server, disables the unit and removes it. It asks
first when sessions are running (`--yes` skips the question): the server's
shutdown hangs up every shell it runs. systemd signals only the server
(`KillMode=process`); if it ignores the signal, systemd SIGKILLs only the
server after `TimeoutStopSec`, and its shells then lose their terminal and
get SIGHUP from the kernel.

## Upgrade

Re-run `install.sh` (it installs the new release and runs `silo up`), or
install a build with `install -m755 silo ~/.local/bin/silo` and run `silo up`.
Both replace the file by rename, so the running server keeps its binary;
do not `cp` over it.

It reloads the unit, whose `ExecReload` runs `silo server --takeover --daemon
--idle-exit 0` inside the service. The successor takes every session over
live (same shells, no hangup), tells systemd `MAINPID=<its pid>` before the
handoff commits and `READY=1` once serving; the old server then exits. The
service stays active, now tracking the successor, and `Restart=on-failure`
applies to it as before. If the takeover fails, the old server keeps serving
and stays the main process; the successor's error is in the journal.

Remote clients are disconnected at the handoff and reconnect to the
successor, which keeps the server's identity and paired devices. Pairing
invites (`silo pair`) live only in the server's memory: any not yet used are
lost on upgrade (and restart), so create a new one afterwards.

Upgrade through the unit (`silo up`), not with a plain `silo server --takeover`
from a shell: outside the service it has no `NOTIFY_SOCKET` and is not in the
unit's cgroup, so systemd cannot follow it (the unit goes inactive when the
old server exits, and the successor lives in your login session).

`systemctl --user restart silo` is not an upgrade: it stops the server first,
which ends every session.
