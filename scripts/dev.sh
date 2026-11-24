#!/usr/bin/env bash
# scripts/dev.sh â€” one-command developer bootstrap for StellarLend.
#
# Usage:
#   ./scripts/dev.sh            Check prerequisites, install npm workspaces, print next steps
#   ./scripts/dev.sh --check    Validate structure only (no installs); used by CI/tests
#
# Environment (test hook):
#   DEV_ROOT   Override the repository root.

set -euo pipefail

REPO_ROOT="${DEV_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
MODE="bootstrap"
[[ "${1:-}" == "--check" ]] && MODE="check"

fail() { echo "dev: ERROR $*" >&2; exit 1; }

# Make sure the one-command entry points exist before doing any work.
for t in dev fmt check lint test ci; do
  grep -qE "^${t}:" "$REPO_ROOT/Makefile" || fail "Makefile is missing target '$t'"
done
for f in justfile package.json scripts/devnet/bootstrap.sh; do
  [[ -f "$REPO_ROOT/$f" ]] || fail "missing $f"
done

if [[ "$MODE" == "check" ]]; then
  echo "dev: structure OK (6 make targets, 3 entry points)"
  exit 0
fi

command -v node >/dev/null 2>&1 || fail "node is required (>=20)"
node_major="$(node -p "process.versions.node.split('.')[0]")"
[[ "$node_major" -ge 20 ]] || fail "node >=20 required (found $node_major)"
command -v npm >/dev/null 2>&1 || fail "npm is required"
command -v cargo >/dev/null 2>&1 || fail "cargo is required (Rust workspace)"
command -v just >/dev/null 2>&1 || fail "just is required (see justfile)"

if [[ ! -d "$REPO_ROOT/node_modules" ]]; then
  echo "dev: installing npm workspaces..."
  (cd "$REPO_ROOT" && npm install --workspaces)
fi

echo "dev: ready. Run 'make help' to list targets, or 'make test'."
