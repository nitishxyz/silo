#!/usr/bin/env bash
# Fetch the pinned CEF binary distribution into the shared dev cache.
# Usage: scripts/fetch-cef.sh   then   export CEF_PATH="$(scripts/fetch-cef.sh --print)"
set -euo pipefail

CEF_CRATE_VERSION="154.5.0+154.0.34"
CEF_VERSION="154.0.34"
CACHE="${HOME}/Library/Caches/silo-dev/cef/${CEF_VERSION}"
TOOLS="${HOME}/Library/Caches/silo-dev/tools"

if [[ "${1:-}" == "--print" ]]; then
  echo "${CACHE}"
  exit 0
fi

if [[ -f "${CACHE}/archive.json" ]]; then
  echo "${CACHE}"
  exit 0
fi

if [[ ! -x "${TOOLS}/bin/export-cef-dir" ]]; then
  cargo install export-cef-dir --version "=${CEF_CRATE_VERSION}" --root "${TOOLS}" --locked >&2
fi

mkdir -p "$(dirname "${CACHE}")"
"${TOOLS}/bin/export-cef-dir" --force --version "${CEF_VERSION}" "${CACHE}" >&2
echo "${CACHE}"
