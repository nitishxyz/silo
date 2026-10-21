#!/bin/zsh
# Serve www/ on localhost and open the page in headless Chrome with a
# `silo1…` ticket in the fragment; prints result.json when the page posts it.
# usage: run.zsh <silo1…ticket> [port] [extra query, e.g. online=0 or all_addrs=1]
set -eu
here=${0:A:h}
ticket=$1
port=${2:-18765}
extra=${3:+&$3}
vt=ghostsnp1-$(<$here/../../GHOSTTY_REV)
chrome=${CHROME:-"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"}
profile=$(mktemp -d /tmp/silo-spike-chrome.XXXXXX)
result=$here/result.json
rm -f $result
typeset -a pids
python3 $here/serve.py $port >/tmp/silo-spike-serve.log 2>&1 &
pids+=($!)
sleep 0.5
"$chrome" --headless=new --user-data-dir=$profile --no-first-run --no-default-browser-check \
  --enable-logging=stderr --v=0 --remote-debugging-port=0 \
  "http://127.0.0.1:$port/?vt=$vt$extra#$ticket" >/tmp/silo-spike-chrome.log 2>&1 &
pids+=($!)
for _ in {1..120}; do
  [[ -s $result ]] && break
  sleep 0.5
done
kill -TERM $pids 2>/dev/null || true
wait $pids 2>/dev/null || true
sleep 1
rm -rf $profile 2>/dev/null || true
if [[ -s $result ]]; then cat $result; else echo "no result; see /tmp/silo-spike-chrome.log" >&2; exit 1; fi
