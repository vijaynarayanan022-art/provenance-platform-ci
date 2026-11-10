# provenance-platform

Rust workspace for the company Code Provenance Platform (see HANDOFF.md).

```
cargo test
```

Git analysis requests rename detection explicitly (`-M`), independent of the user's Git configuration. Eligible lines remain nonblank additions in the selected diff, so a pure rename contributes no eligible lines. A detected rename counts as one changed file at its destination path; unchanged lines keep their prior labels and exclusions at that path. Renames below Git's similarity threshold are treated as a deletion plus an addition.
