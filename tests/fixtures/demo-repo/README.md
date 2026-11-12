# WorktreeFS fixture repo

Small deterministic repository used by the M5 integration test and the M0
benchmark harness. Copy this directory to a scratch location, run
`git init` inside the copy, and commit — the fixture ships working-tree
files only, never a `.git` directory.

Line endings are LF and content is ASCII so checkouts are byte-stable
regardless of `core.autocrlf` on the benchmark machine (the harness
still sets `core.autocrlf=false` for belt and braces).
