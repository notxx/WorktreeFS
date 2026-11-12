# fixture usage

The fixture repo is data for tests, not a runnable program.

- `src/` — three-level nesting with a leaf module (`src/util/math.rs`).
- `docs/` — nested documentation tree (`docs/design/overview.md`).
- `data/metrics.csv` — a small data file with several rows.

Every file is plain ASCII with LF endings; do not add binary or
CRLF-dependent content here, both the dedupe path and the benchmark
rely on byte-stable checkouts.
