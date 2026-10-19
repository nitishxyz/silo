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

_now_ms() { perl -MTime::HiRes=time -e 'printf "%d", time * 1000'; }
ui_perms() { "$DRIVER" perms; }

# Cached pid, revalidated; falls back to a lookup by binary path. Exported
# as UI_PID so the driver posts keys straight to the app (the session-wide
# HID tap can be filtered for some parent terminals).
app_pid() {
    if [[ -z "$APP_PID" ]] || ! kill -0 "$APP_PID" 2>/dev/null; then
        APP_PID=$(pgrep -f "$APP_BIN" | head -1)
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
    : > "$APP_LOG"
    SILO_LOG="${SILO_LOG:-silo_app=debug,info}" nohup python3 -c "import os,sys; os.setsid(); os.execv(sys.argv[1], sys.argv[1:])" "$APP_BIN" >>"$APP_LOG" 2>&1 < /dev/null &
    APP_PID=$!
    disown
    # Window first, then a connected server with a terminal attached.
    read -r WX WY WW WH WID < <("$DRIVER" wait-window "$APP_PID" 10000) || { echo "ui-env: no window" >&2; return 1; }
    wait_log "connected to silo server" 10000 && wait_log "pane attached" 5000
}
app_stop() {
    local p; p=$(app_pid)
    [[ -z "$p" ]] && return 0
    kill "$p" 2>/dev/null
    for _ in $(seq 100); do kill -0 "$p" 2>/dev/null || break; sleep 0.02; done
    APP_PID=""
}
# Stop everything this SILO_HOME started (never the real ~/Library one).
ui_cleanup() {
    app_stop
    target/debug/silo kill-server >/dev/null 2>&1
    local p
    for p in $(pgrep -f 'silo-menubar'); do
        ps eww -p "$p" | grep -q "SILO_HOME=$SILO_HOME" && kill "$p"
    done
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
