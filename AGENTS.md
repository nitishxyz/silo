# Working on this repo

Notes for agents (and people) building and testing locally. Architecture and
design live in [`docs/`](docs); start with [`docs/PLAN.md`](docs/PLAN.md).

## Building and testing: corgi locally, cargo for the app and CI

[corgi](https://corgi.build) is a cargo-compatible build tool with one build
cache per machine (`/Users/Shared/corgi`), so a fresh worktree reuses what any
other checkout already built, and test runs whose inputs did not change are
skipped. It is configured by [`corgi.toml`](corgi.toml); cargo ignores that
file, and CI uses cargo.

```sh
cargo install corgi-build --locked     # once
corgi build -p silo-cli                # target/debug/silo
corgi check -p otto-core
SILO_NET_LOOPBACK=1 corgi test -p silo-proto -p otto-llm   # passing tests are cached
corgi clippy -p silo-server
```

Use **cargo** for the macOS app and its two app-only crates: `silo-app`,
`silo-web` (CEF) and `silo-voice` (whisper.cpp). Their CMake builds and CEF
(which lives outside the workspace, `scripts/fetch-cef.sh`) don't run in
corgi's sandbox yet:

```sh
export CEF_PATH="$(scripts/fetch-cef.sh)"
cargo build -p silo-app
```

Set `SILO_NET_LOOPBACK=1` for `corgi test`: nextest sets it through
`.config/scripts/test-env.sh`, which corgi does not run, and without it the
servers tests start use mDNS and every interface, so macOS asks each rebuilt
test binary for Local Network access.

`scripts/check.sh` (cargo) remains the gate before sending a change. Always
pass `-p` to corgi: a bare `corgi test` takes the whole workspace, app
included. corgi does not read `.config/nextest.toml`, so the wall-clock tests
it serializes (`silo-latency`, `silo-transport` `net::`, …) can fail under
load in a big `corgi test`; rerun them on their own before suspecting a
change.

## Keeping builds corgi-compatible

corgi runs build scripts and proc macros in a sandbox (no network, no ambient
environment, no `target/`, `.git` or `Cargo.lock`) and rejects artifacts that
embed the checkout's absolute path. So:

- **No `env!("CARGO_MANIFEST_DIR")` in code that ends up in an artifact.** In
  tests use paths relative to the package directory (tests run there under
  both tools); in bins and examples read `std::env::var_os("CARGO_MANIFEST_DIR")`
  at run time (`cargo run` sets it); in build scripts read it with
  `std::env::var_os`, not `env!`.
- **Files a crate reads beyond its own `src/**/*.rs`** (fixtures,
  `include_str!` assets, JSON, anything in another crate) go under
  `[extra-inputs]` in `corgi.toml`, or corgi will miss changes to them and
  serve stale builds or cached test passes.
- **Environment variables a build script reads** need an `[env.NAME]` entry.
- **Build scripts must not download or write outside `OUT_DIR`.** Native
  inputs that need the network (libghostty-vt, libneedle) are prepared by
  `scripts/corgi-env.sh`, which corgi runs outside the sandbox; follow that
  pattern for new ones.

The compiler is pinned in [`rust-toolchain.toml`](rust-toolchain.toml); bump it
in its own commit (it rebuilds everything once).
