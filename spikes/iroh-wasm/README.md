# iroh-wasm spike (P7-8)

A browser page that dials a native `silo` server over n0 relays with iroh 1.3.0 compiled
to `wasm32-unknown-unknown`, speaks the real silo protocol (`silo/ctl/1`, `ClientHello`
with a `silo1…` invite, `session.list`, `session.view`, `data.connect`, then a second bi
stream `DataHello` → `Welcome` → snapshot), and decodes the snapshot with libghostty-vt's
own wasm build. Results are in `docs/PHASE7.md` §13.

It is a standalone cargo package (its own `[workspace]`), not part of the silo workspace.

## Needs

- rustup's toolchain with `rustup target add wasm32-unknown-unknown` (Homebrew's `rustc`
  has no wasm std; the scripts put `~/.cargo/bin` first).
- Homebrew `llvm` (clang + llvm-ar) for `ring`'s C code. Apple clang compiles it, but the
  link then fails with undefined `ring_core_*` symbols.
- `wasm-bindgen-cli` matching `Cargo.lock` (0.2.129), installed locally:
  `cargo install wasm-bindgen-cli --version '=0.2.129' --locked --root target/spikes/bin`.
- zig 0.16.0 at `target/tools/` and the ghostty source at `target/ghostty/<rev>/source`
  (both from `scripts/build-ghostty.sh`).
- Google Chrome (headless) and python3.

## Run

From the repo root:

```sh
spikes/iroh-wasm/build.sh        # iroh client -> www/pkg (prints wasm sizes)
spikes/iroh-wasm/build-vt.sh     # libghostty-vt -> www/vt/ghostty-vt.wasm

# A throwaway server with relays (never the real SILO_HOME):
export SILO_HOME=/tmp/silo-wasm-spike
cargo build -p silo-cli          # with SILO_GHOSTTY_RUNTIME / CEF_PATH set as usual
target/debug/silo server --relay default --no-mdns --idle-exit 0 &
target/debug/silo send <block> 'echo hello' -e   # something to snapshot (`silo ls` for ids)

# Each run redeems a fresh device invite (they are single-use):
spikes/iroh-wasm/run.zsh "$(target/debug/silo pair --ttl 10m)"                  # waits for home relay
spikes/iroh-wasm/run.zsh "$(target/debug/silo pair --ttl 10m)" 18765 online=0   # dials at once
spikes/iroh-wasm/run.zsh "$(target/debug/silo pair --ttl 10m)" 18765 all_addrs=1 # keep the ticket's IPs
```

`run.zsh` serves `www/` on `127.0.0.1:<port>` (`serve.py`), opens
`/?vt=<VT>#<ticket>` in headless Chrome with a temp profile, waits for the page to POST
its result, prints `result.json` and stops both processes. The ticket is only in the
fragment; `serve.py`'s log shows it never reaches the HTTP server.

Native comparison (same code, tokio): `cargo run --release -- <ticket> ghostsnp1-<rev>` in
this directory (`SPIKE_NO_ONLINE=1`, `SPIKE_ALL_ADDRS=1`, `SPIKE_SNAPSHOT_OUT=<file>`).

Use `CARGO_TARGET_DIR=target/spikes` (the scripts do), and stop the server with SIGINT/SIGTERM.
