# Helpers for driving silo-app in UI smoke tests: `source scripts/ui-env.sh`.
# Needs Accessibility + Screen Recording permission for the calling terminal.
DRIVER="${DRIVER:-target/ui-driver}"
[[ -x "$DRIVER" ]] || swiftc -O scripts/ui-driver.swift -o "$DRIVER"
export SILO_HOME="${SILO_HOME:-/tmp/silo-ui}"
APP_LOG="${APP_LOG:-/tmp/silo-ui.log}"

app_pid() { pgrep -f "${APP_BIN:-target/debug/silo-app}" | head -1; }
# Start the app detached from this shell (survives the calling command).
app_start() {
    app_stop
    : > "$APP_LOG"
    SILO_LOG="${SILO_LOG:-silo_app=debug,info}" nohup python3 -c "import os,sys; os.setsid(); os.execv(sys.argv[1], sys.argv[1:])" "${APP_BIN:-target/debug/silo-app}" >>"$APP_LOG" 2>&1 < /dev/null &
    disown
    for _ in $(seq 50); do "$DRIVER" window "$(app_pid)" >/dev/null 2>&1 && break; sleep 0.2; done
    sleep 1
}
app_stop() { local p; p=$(app_pid); [[ -n "$p" ]] && kill "$p" && sleep 0.5; return 0; }
front() { osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $(app_pid)) to true" >/dev/null; sleep 0.3; }
win() { read -r WX WY WW WH < <("$DRIVER" window "$(app_pid)"); }
# Window-relative point -> global.
at() { echo $((WX + $1)) $((WY + $2)); }
shot() { win; screencapture -x -R"$WX,$WY,$WW,$WH" "$1"; }
# keys TEXT [MODS], e.g. keys t "command down"; keycode 36 ["shift down"]
keys() { local m=""; [[ -n "${2:-}" ]] && m=" using {$2}"; osascript -e "tell application \"System Events\" to keystroke \"$1\"$m"; }
keycode() { local m=""; [[ -n "${2:-}" ]] && m=" using {$2}"; osascript -e "tell application \"System Events\" to key code $1$m"; }
log_clean() { sed 's/\x1b\[[0-9;]*m//g' "$APP_LOG" | tr -d '\000'; }
