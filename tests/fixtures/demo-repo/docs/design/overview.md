# design overview

The fixture models a tiny multi-directory codebase:

- `README.md` at the root (single-level lookup).
- `docs/design/overview.md` (two-level nesting below `docs/`).
- `src/util/math.rs` (two-level nesting below `src/`).

The M0 integration test walks all of these through the WinFSP mount to
exercise lower reads, nested-directory enumeration, and copy-up.
