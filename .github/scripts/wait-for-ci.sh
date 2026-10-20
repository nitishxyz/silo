#!/bin/sh
# Wait for the `ci` workflow to pass on a commit (release.yml's publish
# gate). Exits 0 once a `ci` push run for the commit has succeeded; 1 when
# every such run has finished without success (failed or cancelled), when
# none exists within CI_GRACE seconds, or after TIMEOUT seconds.
# A re-run CI attempt counts: the run reports its latest attempt.
# Usage: wait-for-ci.sh SHA [TIMEOUT]   (gh authenticated, `actions: read`;
# GH_REPO or the current checkout names the repository)
set -eu

sha=$1
timeout=${2:-2700}
grace=${CI_GRACE:-600}
interval=${CI_POLL_INTERVAL:-30}
start=$(date +%s)

while :; do
  elapsed=$(($(date +%s) - start))
  if runs=$(gh api "repos/{owner}/{repo}/actions/workflows/ci.yml/runs?head_sha=$sha&event=push&per_page=100" \
    --jq '.workflow_runs[] | "\(.id) \(.status) \(.conclusion // "-") attempt=\(.run_attempt) \(.head_branch) \(.html_url)"'); then
    if [ -z "$runs" ]; then
      echo "no ci run for $sha yet (${elapsed}s)"
      if [ "$elapsed" -ge "$grace" ]; then
        echo "no ci push run exists for $sha; push the commit to a branch (docs-only commits skip ci)" >&2
        exit 1
      fi
    else
      printf '%s\n' "$runs"
      if printf '%s\n' "$runs" | grep -q '^[0-9]* completed success '; then
        echo "ci passed on $sha"
        exit 0
      fi
      if ! printf '%s\n' "$runs" | grep -vq '^[0-9]* completed '; then
        echo "ci did not pass on $sha; re-run it, then re-run this workflow's failed jobs" >&2
        exit 1
      fi
    fi
  else
    echo "querying ci runs failed; retrying" >&2
  fi
  if [ "$elapsed" -ge "$timeout" ]; then
    echo "ci on $sha did not finish within ${timeout}s" >&2
    exit 1
  fi
  sleep "$interval"
done
