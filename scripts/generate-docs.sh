#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

cargo run --quiet --bin docsgen

# Format generated docs to pass mdformat --check in CI
if command -v mdformat &>/dev/null; then
  mdformat docs/reference/cli.md docs/reference/config.md
fi
