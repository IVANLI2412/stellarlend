#!/usr/bin/env bash
# Unit tests for scripts/no-println.sh — pure shell, no Rust toolchain needed.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="$SCRIPT_DIR/no-println.sh"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

roots="$tmp/contracts"
mkdir -p "$roots"

pass=0
fail=0

assert_exit() {
  local expected="$1" desc="$2"
  set +e
  CONTRACT_ROOTS_OVERRIDE="$roots" bash "$SCRIPT" --dir "$tmp" >"$tmp/out" 2>&1
  local got=$?
  set -e
  if [[ "$got" -eq "$expected" ]]; then
    echo "PASS: $desc"
    pass=$((pass + 1))
  else
    echo "FAIL: $desc (expected $expected, got $got)"
    cat "$tmp/out"
    fail=$((fail + 1))
  fi
}

printf 'fn ok() { let x = 1; }\n' > "$roots/clean.rs"
assert_exit 0 "clean source passes"

printf '// println! mentioned in a doc comment\nfn ok() {}\n' > "$roots/comment.rs"
assert_exit 0 "comment mention passes"

printf 'fn bad() { println!("debug"); }\n' > "$roots/bad.rs"
assert_exit 1 "println! is rejected"

rm -f "$roots/bad.rs"
assert_exit 0 "removing the violation fixes the guard"

echo "no-println tests: $pass pass, $fail fail"
[[ "$fail" -eq 0 ]]
