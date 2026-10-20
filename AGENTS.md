# Agent instructions: remote browser experiment

This branch (`experiment/remote-browser`) is for Spike A only. Read `docs/REMOTE_BROWSER.md` first; it is the
design and the plan. Production Silo work happens on `main`, which is not yours to change.

## Machines

| Machine | Role | Notes |
|---|---|---|
| AWS `silo-build` (`mac-m4pro.metal`) | Chromium/CEF checkout and builds; the patched worker for A2 | 14 cores, 48 GB RAM, 1 TB encrypted volume, macOS 26 Tahoe. Billed hourly while allocated |
| Mac mini (`batmini.local`) | Live server for A4–A6 | Runs the user's real Silo server. Read-only unless the user says otherwise |
| User's MacBook | Client: replay harness and compositor (A3–A6) | Runs the user's real Silo app. Never modify its install |

## Scope and order

1. **A0 source audit first, on paper.** Fetch only the Chromium/CEF sources needed to read the code, matching our
   CEF line (`cef =154.5.0` / CEF `154.0.34`, see `crates/silo-web`). Write the findings into
   `docs/REMOTE_BROWSER.md` §4/§12: cut-point candidates for T and V, what each serializes, estimated patch
   footprint, and anything that kills an option. Stop and report before any full build.
2. **Full build only after the user approves the A0 report.** One arm64 build directory, release-style flags,
   no debug symbols beyond what is needed, no second architecture.
3. Then A1–A7 in order. Each step ends with a short written result in the doc and a report to the user.

## Rules

- **Disk:** check `df -h` before and after every large fetch or build. Keep at least 100 GB free. Never keep two
  Chromium build directories. Report usage in every step result.
- **Long jobs:** run `gclient sync`, `gn`, `ninja` and CEF's `automate-git.py` in a persistent terminal or
  `tmux`, never as a one-shot command that can time out. Log to a file. Never SIGKILL; stop with SIGTERM/Ctrl-C.
- **Git:** commit and push only to `experiment/remote-browser` (or `experiment/remote-browser-*` sub-branches).
  Never push to `main`, never force-push, never touch tags or releases. Never commit Chromium/CEF sources or
  build outputs; patches go in as `.patch` files with a script that applies them.
- **No fallbacks or compat code:** one clean path. Experiments are throwaway, but say clearly what is proven
  and what is not.
- **Credentials:** never copy cookies, profiles, keychains or tokens between machines. Logins (e.g. Gmail) only
  happen inside the worker, by the user, when the user chooses to. Never print secrets.
- **The user's Silo:** don't stop, upgrade or reconfigure the user's Silo servers, apps, launchd agents or
  `SILO_HOME` on any machine. Test servers use a temporary `SILO_HOME`.
- **AWS:** don't create, stop or release AWS resources from this machine. The user manages billing.
- **Reporting:** concise. Lead with the outcome, then evidence (numbers, file:line in Chromium, screenshots),
  then open questions. No marketing claims; label estimates as estimates.
