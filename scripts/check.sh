#!/usr/bin/env bash
# Local check: fmt, clippy, then the tests of the crates changed since BASE
# plus every crate depending on them (cargo-nextest: `brew install
# cargo-nextest`).
#
#   scripts/check.sh               changed crates (BASE: merge base with origin/main)
#   BASE=HEAD scripts/check.sh     only uncommitted changes
#   scripts/check.sh --all         every test in the workspace
#   scripts/check.sh --no-clippy --no-test --doc --sweep
#   scripts/check.sh -- <args>     passed on to `cargo nextest run`
#
# Builds always cover the whole workspace, so every run (and plain
# `cargo test --workspace`) shares one feature set and one set of artefacts;
# only the tests that run are filtered.
set -euo pipefail
cd "$(dirname "$0")/.."

all=0 clippy=1 test=1 doc=0 sweep=0
nextest_args=()
while (($#)); do
    case $1 in
        --all) all=1 ;;
        --no-clippy) clippy=0 ;;
        --no-test) test=0 ;;
        --doc) doc=1 ;;
        --sweep) sweep=1 ;;
        --) shift; nextest_args=("$@"); break ;;
        *) sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 2 ;;
    esac
    shift
done

step() { printf '\n==> %s\n' "$*" >&2; }

if ((sweep)); then
    step "cargo sweep: artefacts unused for 7 days"
    cargo sweep --time 7
fi

step "cargo fmt --check"
cargo fmt --all -- --check

if ((clippy)); then
    step "cargo clippy"
    cargo clippy --workspace --all-targets -- -D warnings
fi

if ((test)); then
    filter=""
    if ((!all)); then
        base=${BASE:-$(git merge-base HEAD origin/main 2>/dev/null || echo HEAD)}
        changed=$({ git diff --name-only "$base"; git ls-files --others --exclude-standard; } | sort -u)
        # Anything shaping every build means every test.
        if grep -qE '^(Cargo\.(toml|lock)|\.cargo/|\.config/nextest\.toml|rust-toolchain)' <<<"$changed"; then
            all=1
        else
            for dir in $(sed -n 's#^crates/\([^/]*\)/.*#\1#p' <<<"$changed" | sort -u); do
                [[ -f crates/$dir/Cargo.toml ]] || continue
                name=$(sed -n 's/^name = "\(.*\)"/\1/p' "crates/$dir/Cargo.toml" | head -n 1)
                filter+="${filter:+ | }rdeps(=$name)"
            done
        fi
    fi
    if ((all)); then
        step "cargo nextest run: workspace"
        cargo nextest run --workspace ${nextest_args[@]+"${nextest_args[@]}"}
    elif [[ -n $filter ]]; then
        step "cargo nextest run: $filter"
        cargo nextest run --workspace -E "$filter" ${nextest_args[@]+"${nextest_args[@]}"}
    else
        step "no crate changed since $base: no tests"
    fi
fi

if ((doc)); then
    step "doctests"
    cargo test --workspace --doc
fi
