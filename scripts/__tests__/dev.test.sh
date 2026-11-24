#!/usr/bin/env bash
# Unit tests for scripts/dev.sh â€” no Rust/Node builds required.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="$HERE/dev.sh"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/scripts/devnet"

pass=0
fail=0

make_valid() {
  : > "$tmp/Makefile"
  for t in dev fmt check lint test ci; do
    printf '%s:\n\t@true\n' "$t" >> "$tmp/Makefile"
  done
}

assert_exit() {
  local expected="$1" desc="$2"
  set +e
  DEV_ROOT="$tmp" bash "$SCRIPT" --check >"$tmp/out" 2>&1
  local got=$?
  set -e
  if [[ "$got" -eq "$expected" ]]; then
    echo "PASS: $desc"; pass=$((pass + 1))
  else
    echo "FAIL: $desc (expected $expected, got $got)"; cat "$tmp/out"; fail=$((fail + 1))
  fi
}

make_valid
touch "$tmp/justfile" "$tmp/package.json" "$tmp/scripts/devnet/bootstrap.sh"
assert_exit 0 "valid structure passes --check"

printf 'dev:\n\t@true\n' > "$tmp/Makefile"
assert_exit 1 "missing targets are rejected"

make_valid
rm -f "$tmp/scripts/devnet/bootstrap.sh"
assert_exit 1 "missing entry point is rejected"

echo "dev tests: $pass pass, $fail fail"
[[ "$fail" -eq 0 ]]
