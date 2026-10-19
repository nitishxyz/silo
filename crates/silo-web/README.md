# silo-web

CEF (windowless, accelerated paint) for browser blocks. macOS only.

## CEF binaries

- Pinned: `cef = "=154.5.0"` (CEF 154.0.34, Chromium 154.0.8037.98).
- `scripts/fetch-cef.sh` downloads the minimal distribution once, via cef-rs's `export-cef-dir`, into
  `~/Library/Caches/silo-dev/cef/154.0.34` (~340 MB unpacked).
- Every build of this crate needs `CEF_PATH` set to that directory:
  `export CEF_PATH="$(scripts/fetch-cef.sh)"`.
  - Without it, `cef-dll-sys` downloads its own copy into `OUT_DIR`.
- `cef-dll-sys` builds `libcef_dll_wrapper` with CMake and Ninja (`brew install cmake ninja`).

## Bundle layout

CEF only starts from inside an app bundle (`Layout::for_main`). The helper name comes from the
`.app` name:

```text
Silo.app/Contents/MacOS/<main exe>
Silo.app/Contents/Frameworks/Chromium Embedded Framework.framework
Silo.app/Contents/Frameworks/Silo Helper.app/Contents/MacOS/Silo Helper
Silo.app/Contents/Frameworks/Silo Helper (GPU).app/Contents/MacOS/Silo Helper (GPU)
Silo.app/Contents/Frameworks/Silo Helper (Renderer).app/Contents/MacOS/Silo Helper (Renderer)
Silo.app/Contents/Frameworks/Silo Helper (Plugin).app/Contents/MacOS/Silo Helper (Plugin)
```

- Each helper executable is a copy of the `silo-web-helper` bin, which calls `silo_web::run_helper()`.
- Each helper's `Info.plist` sets `CFBundleExecutable`, a unique `CFBundleIdentifier` and
  `LSUIElement=1`.
- The sandbox is off (`no_sandbox`). The `sandbox` cargo feature is not enabled.

## Smoke test

`cargo run -p silo-web --example offscreen` needs a logged-in GUI session; no window is shown.

- It builds `target/<profile>/examples/silo-web-dev/SiloWebOffscreen.app`. The helpers are copies
  of the example binary, and the framework is a symlink into `CEF_PATH`.
- It re-execs itself inside that bundle.
- It checks IOSurface pixels after the first paint, a mouse click, a key press and a resize.
