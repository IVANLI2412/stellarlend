#!/usr/bin/env bash
# scripts/no-println.sh
#
# Guard that rejects debug `println!` macros in contract Rust sources.
# Wired into `.husky/pre-push` so debug prints never reach the shared branch.
#
# Usage:
#   ./scripts/no-println.sh [--staged] [--dir PATH]
#
# Modes:
#   (default)  Scan every *.rs file under the contract roots (full audit / CI).
#   --staged   Scan only staged *.rs files under the contract roots (pre-push).
#
# Options:
#   --dir PATH  Override the repository root (used by the test suite).
#
# Environment (test hook):
#   CONTRACT_ROOTS_OVERRIDE  Colon-separated contract roots to scan instead of
#                            the defaults (<repo>/stellar-lend/contracts and
#                            <repo>/contracts).
#
# Exit code: 0 = clean, 1 = violations found, 2 = bad invocation.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MODE="all"
ROOTS_OVERRIDE="${CONTRACT_ROOTS_OVERRIDE:-}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --staged) MODE="staged"; shift ;;
    --dir) REPO_ROOT="$2"; shift 2 ;;
    *) echo "no-println: unknown option '$1'" >&2; exit 2 ;;
  esac
done

if [[ -n "$ROOTS_OVERRIDE" ]]; then
  IFS=':' read -r -a roots <<< "$ROOTS_OVERRIDE"
else
  roots=("$REPO_ROOT/stellar-lend/contracts" "$REPO_ROOT/contracts")
fi

files=()
if [[ "$MODE" == "staged" ]]; then
  while IFS= read -r f; do
    [[ -z "$f" ]] && continue
    abs="$REPO_ROOT/$f"
    [[ -f "$abs" ]] || continue
    for r in "${roots[@]}"; do
      case "$abs" in
        "$r"/*) files+=("$abs"); break ;;
      esac
    done
  done < <(git -C "$REPO_ROOT" diff --cached --name-only --diff-filter=ACM -- '*.rs' 2>/dev/null || true)
else
  for r in "${roots[@]}"; do
    [[ -d "$r" ]] || continue
    while IFS= read -r f; do
      [[ -n "$f" ]] && files+=("$f")
    done < <(find "$r" -type f -name '*.rs' -print 2>/dev/null || true)
  done
fi

violations=0
checked=0
if [[ ${#files[@]} -gt 0 ]]; then
  for file in "${files[@]}"; do
    [[ -f "$file" ]] || continue
    checked=$((checked + 1))
    rel="${file#$REPO_ROOT/}"
    while IFS=: read -r lineno text; do
      [[ -z "$lineno" ]] && continue
      trimmed="${text#"${text%%[![:space:]]*}"}"
      # A mention in a comment is documentation, not debug output.
      [[ "$trimmed" == //* ]] && continue
      echo "ERROR $rel:$lineno contains println!() — remove it before pushing" >&2
      violations=$((violations + 1))
    done < <(grep -n 'println!' "$file" 2>/dev/null || true)
  done
fi

if [[ "$violations" -gt 0 ]]; then
  echo "no-println: $violations violation(s) in $checked file(s). Bypass with: git push --no-verify" >&2
  exit 1
fi

echo "no-println: clean ($checked file(s) checked)"
exit 0
