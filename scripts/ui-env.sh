# Helpers for driving silo-app in UI smoke tests: `source scripts/ui-env.sh`.
# Needs Accessibility + Screen Recording permission for the calling terminal
# (`ui_perms` reports both). Always runs against a throwaway SILO_HOME.
#
#   app_start      reuse a running, windowed app; start one otherwise
#   app_restart    always start a fresh app
#   front          bring it to the front;  win: refresh WX WY WW WH
#   typ TEXT       type text ("\n" = Return), one driver call
#   chord MODS K   e.g. chord cmd t; chord cmd,shift d; keycode 36 [mods]
#   shot FILE      capture the window only (-l WID);  ui_cleanup: stop app, server, menubar
#   menubar_stop   stop this SILO_HOME's menu-bar agent (app_restart does too: the
#                  agent outlives the app, and a wiped SILO_HOME loses its lock)
# Every helper acts only on what this SILO_HOME launched: app_restart records
# the app's pid in $SILO_HOME/.ui-env-app.pid, and a process counts only while
# it runs APP_BIN with exactly this SILO_HOME in its environment (read entry by
# entry with `ui-driver env`, not from `ps eww` text); if the home was wiped,
# that is how the app is found. The menu-bar agent and server are matched the
# same way. Agents using other homes, even with the same bundle, are never
# found, focused or stopped.
# SILO_MOTION_SLOWDOWN=12 app_restart stretches animations for frame shots.
# The first click after `front` may only activate the window.
DRIVER="${DRIVER:-target/ui-driver}"
_driver_sum=$(shasum scripts/ui-driver.swift | cut -c1-40)
if [[ ! -x "$DRIVER" || "$(cat "$DRIVER.sum" 2>/dev/null)" != "$_driver_sum" ]]; then
    swiftc -O scripts/ui-driver.swift -o "$DRIVER" 2>/dev/null && echo "$_driver_sum" > "$DRIVER.sum" || return 1
fi
export SILO_HOME="${SILO_HOME:-/tmp/silo-ui}"
case "$SILO_HOME" in /tmp/* | /private/tmp/*) ;; *) echo "ui-env: SILO_HOME must be under /tmp" >&2; return 1 ;; esac
# Browser panes need CEF's bundle layout: run the app from a dev bundle.
APP_BIN="${APP_BIN:-$(scripts/dev-bundle.sh)}" || return 1
APP_LOG="${APP_LOG:-/tmp/silo-ui.log}"
APP_PID="${APP_PID:-}"
APP_PIDFILE="$SILO_HOME/.ui-env-app.pid"

_now_ms() { perl -MTime::HiRes=time -e 'printf "%d", time * 1000'; }
ui_perms() { "$DRIVER" perms; }

# Whether PID runs with exactly this SILO_HOME.
_in_home() { [[ "$("$DRIVER" env "$1" SILO_HOME 2>/dev/null)" == "$SILO_HOME" ]]; }
# Whether PID is this home's app: alive, running APP_BIN, with this SILO_HOME.
_ours() {
    [[ -n "$1" ]] && kill -0 "$1" 2>/dev/null || return 1
    [[ "$(ps -o command= -p "$1")" == "$APP_BIN"* ]] || return 1
    _in_home "$1"
}
# This home's app: the cached pid, else the recorded one, else (home wiped)
# the APP_BIN process with this SILO_HOME. Exported as UI_PID so the driver
# posts keys straight to the app (the session-wide HID tap can be filtered
# for some parent terminals).
app_pid() {
    if ! _ours "$APP_PID"; then
        APP_PID=$(cat "$APP_PIDFILE" 2>/dev/null)
        _ours "$APP_PID" || APP_PID=$(for p in $(pgrep -f "$APP_BIN"); do _ours "$p" && echo "$p"; done | head -1)
    fi
    export UI_PID="$APP_PID"
    echo "$APP_PID"
}
# Poll until PATTERN appears in the app log (default timeout 10s).
wait_log() {
    local deadline=$(($(_now_ms) + ${2:-10000}))
    until grep -aq -- "$1" "$APP_LOG" 2>/dev/null; do
        (($(_now_ms) > deadline)) && { echo "ui-env: timed out waiting for '$1'" >&2; return 1; }
        sleep 0.05
    done
}
app_running() { local p; p=$(app_pid); [[ -n "$p" ]] && "$DRIVER" window "$p" >/dev/null 2>&1; }
app_start() {
    if [[ -z "${APP_RESTART:-}" ]] && app_running; then win; return 0; fi
    app_restart
}
app_restart() {
    app_stop
    menubar_stop
    : > "$APP_LOG"
    SILO_LOG="${SILO_LOG:-silo_app=debug,info}" nohup python3 -c "import os,sys; os.setsid(); os.execv(sys.argv[1], sys.argv[1:])" "$APP_BIN" >>"$APP_LOG" 2>&1 < /dev/null &
    APP_PID=$!
    disown
    mkdir -p "$SILO_HOME" && echo "$APP_PID" > "$APP_PIDFILE"
    # Window first, then a connected server with a terminal attached.
    read -r WX WY WW WH WID < <("$DRIVER" wait-window "$APP_PID" 10000) || { echo "ui-env: no window" >&2; return 1; }
    wait_log "connected to silo server" 10000 && wait_log "pane attached" 5000
}
app_stop() {
    local p; p=$(app_pid)
    rm -f "$APP_PIDFILE"
    APP_PID=""
    [[ -z "$p" ]] && return 0
    kill "$p" 2>/dev/null
    for _ in $(seq 100); do kill -0 "$p" 2>/dev/null || break; sleep 0.02; done
}
# Processes matching PATTERN whose environment has exactly this SILO_HOME
# (never another home's, never the real one).
home_pids() {
    local p
    for p in $(pgrep -f "$1"); do
        _in_home "$p" && echo "$p"
    done
}
# SIGTERM this home's processes matching PATTERN and wait for them.
_home_stop() {
    local p pids
    pids=$(home_pids "$1")
    [[ -z "$pids" ]] && return 0
    for p in $pids; do kill "$p" 2>/dev/null; done
    for _ in $(seq 150); do [[ -z "$(home_pids "$1")" ]] && return 0; sleep 0.02; done
    echo "ui-env: still running for $SILO_HOME: $(home_pids "$1" | tr '\n' ' ')" >&2
}
menubar_pids() { home_pids 'silo-menubar'; }
menubar_stop() { _home_stop 'silo-menubar'; }
# Wait (without signalling) for this home's processes matching PATTERN to exit.
_home_wait() {
    for _ in $(seq 250); do [[ -z "$(home_pids "$1")" ]] && return 0; sleep 0.02; done
    echo "ui-env: still running for $SILO_HOME: $(home_pids "$1" | tr '\n' ' ')" >&2
}
# The server pid recorded in this home's runtime.json, if it runs with this home.
_server_pid() {
    local p
    p=$(sed -n 's/.*"pid"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$SILO_HOME/runtime.json" 2>/dev/null | head -1)
    [[ -n "$p" ]] && kill -0 "$p" 2>/dev/null && _in_home "$p" && echo "$p"
}
# Stop everything this SILO_HOME started (never the real ~/Library one): the
# app, its server and the menu-bar agent. The server gets SIGTERM (its pid
# from runtime.json, else matched by home if the home was wiped under it):
# `silo kill-server` is an owner action that would ask for approval. Its
# pty-hosts exit once their programs are hung up; they are waited for.
ui_cleanup() {
    app_stop
    local p; p=$(_server_pid)
    if [[ -n "$p" ]]; then
        kill "$p" 2>/dev/null
        for _ in $(seq 250); do kill -0 "$p" 2>/dev/null || break; sleep 0.02; done
    fi
    _home_stop 'silo server'
    _home_wait 'pty-host'
    menubar_stop
    return 0
}
front() {
    "$DRIVER" front "$(app_pid)" && return 0
    osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $(app_pid)) to true" >/dev/null
}
win() { read -r WX WY WW WH WID < <("$DRIVER" window "$(app_pid)"); }
# Window-relative point -> global.
at() { echo $((WX + $1)) $((WY + $2)); }
# The window alone (-l): unaffected by other windows or dialogs over it.
shot() { win; screencapture -x -o -l"$WID" "$1"; }

typ() { app_pid >/dev/null; "$DRIVER" type "$1"; }
# chord MODS KEY: KEY is a character (US layout) or a virtual key code.
chord() { app_pid >/dev/null; "$DRIVER" key "$2" "$1"; }
# Legacy forms: keys TEXT ["command down, shift down"]; keycode N [mods].
_mods() { echo "${1:-}" | sed -e 's/command down/cmd/g' -e 's/shift down/shift/g' -e 's/control down/ctrl/g' -e 's/option down/alt/g' -e 's/[ ]//g'; }
keys() { if [[ -n "${2:-}" ]]; then chord "$(_mods "$2")" "$1"; else typ "$1"; fi; }
keycode() { app_pid >/dev/null; "$DRIVER" key "$1" "$(_mods "${2:-}")"; }
log_clean() { sed 's/\x1b\[[0-9;]*m//g' "$APP_LOG" | tr -d '\000'; }
# Brightness of a window-relative rect, sampled N times every ~MS ms:
# `blink_samples X Y W H N MS` -> "12 140 13 141 ..."
blink_samples() {
    win; local out=()
    for _ in $(seq "$5"); do
        screencapture -x -R"$((WX + $1)),$((WY + $2)),$3,$4" /tmp/silo-sample.png
        out+=("$("$DRIVER" mean /tmp/silo-sample.png)")
        sleep "$(echo "$6 / 1000" | bc -l)"
    done
    echo "${out[*]}"
}
