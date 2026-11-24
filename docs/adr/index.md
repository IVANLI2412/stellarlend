# Architecture Decision Records (ADR) Index

This directory contains Architecture Decision Records for the StellarLend protocol.
Each ADR documents a significant architectural decision, its context, and consequences.

## ADR List

| ID | Title | Status | Date | Authors |
|----|-------|--------|------|---------|
| 0001 | [Example: Use Rust 2021 edition for contracts](template.md) | Proposed | YYYY-MM-DD | [Author] |

---

## How to Add a New ADR

1. Copy `template.md` to a new file named `XXXX-short-title.md` (next sequential number).
2. Fill in all sections; replace placeholder values.
3. Set **Status** to `Proposed` initially.
4. Open a PR; reviewers approve by changing status to `Accepted`.
5. Once merged, the ADR is immutable. To reverse or replace, create a new ADR with status `Superseded` referencing the old one.

## Validation

A CI workflow (`.github/workflows/adr-validate.yml`) runs on every PR to ensure:

- Filename matches `NNNN-kebab-case-title.md` pattern.
- Required front-matter fields present: `Status`, `Date`, `Authors`.
- Status is one of: `Proposed`, `Accepted`, `Superseded`, `Deprecated`.
- Date format is `YYYY-MM-DD`.
- No duplicate IDs.

## References

- [ADR GitHub Organization](https://github.com/joelparkerhenderson/architecture-decision-record)
- [Michael Nygard — Documenting Architecture Decisions](https://cognitect.com/blog/2011/11/15/documenting-architecture-decisions)