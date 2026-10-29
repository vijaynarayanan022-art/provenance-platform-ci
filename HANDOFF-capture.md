# Capture R1c handoff

## What exists

`prov-hook` is a silent `codex` hook executable. It reads JSON from stdin, hashes command text without retaining it, catches panics, and exits successfully. `prov-capture` discovers the Git root and git directory by walking upward from `cwd` and reading `.git` and `HEAD` directly. Repo identity is cached using Git directory creation time where available, with a first-parent-root check fallback. Warm normal events use one Git process (`status`); a dirty tracked set may add one `cat-file --batch`, and HEAD movement uses one name-only `diff` plus a commit-header read. There are no per-file `git show` calls.

The scanner handles Git's porcelain-v2 dirty set under a per-repository lock. It records external/AI changes, head moves, bulk changes, windows, overlaps, deduplication, and degraded markers. It caps normal file processing at 500 dirty paths. Oversized, binary, symlink, and excluded paths are never snapshotted. Excluded text paths get hash-only shadow entries and events with null line ranges. Deletions leave tombstones while the path remains dirty, so the event is emitted once.

The hook does not read transcript fields, write inside the worktree, or invoke mutating Git operations. Git calls use `GIT_OPTIONAL_LOCKS=0`; status and path-list outputs are NUL-delimited. Known-repository markers use `repos/<repo_id>/markers.jsonl`; unknown-repository markers use `inbox.jsonl`.

## On-disk formats and events

Store root is `PROV_HOME` when set. Otherwise it is `%LOCALAPPDATA%\\prov` on Windows, `~/Library/Application Support/prov` on macOS, or `$XDG_DATA_HOME/prov` (default `~/.local/share/prov`) on other Unix systems.

- `repo-cache/<sha256-gitdir>.json` contains `repo_id`, `root_check`, and optional `git_dir_created_ns`. If the Git directory creation time is unavailable, the first-parent root is rechecked against `root_check`.
- `repos/<repo_id>/blobs/<sha256>` contains content-addressed text snapshots. Blob writes use same-directory temporary files, flush, and atomic rename/replacement without `sync_all`. A blob read verifies its SHA-256; a mismatch is treated as missing and a subsequent write repairs it.
- `repos/<repo_id>/events/<session_id>.jsonl` contains append-only `prov.event/0.1` records. Appends use `write_all`; a torn tail is separated by a newline before the next append.
- `repos/<repo_id>/markers.jsonl` contains known-repository lock, time-cap, shallow-skip, and other marker events. `inbox.jsonl` is used when no repo id is known.
- `repos/<repo_id>/shadow/<sha256-gitdir>.json` stores dirty-file entries with `size`, `mtime_ns`, `sha256`, `excluded`, and `deleted`. Excluded entries never have blobs. Deletion tombstones retain the previous hash and have `deleted: true` while the deletion remains dirty. Clean paths are removed from shadow state.
- `repos/<repo_id>/state/repo.json` contains active windows (`session_id`, `tool_use_id`, open time, HEAD/branch, overlap, optional bulk count), at most 1,000 recent dedupe keys, and `scan_incomplete_pending`.
- `external_change` and `ai_change` records carry hashes and line metadata. `line_ranges` contains only new-file lines inserted or replaced; pure deletions have no changed line range. `deleted_at` is the sorted list of new-file line positions after which lines were deleted, where zero means before the first line. Ranges never exceed the new file's line count. Unknown prior content yields null line ranges.
- `bulk_change` has `dirty_count`, `degraded: true`, and `reason: "bulk_change"`. One event is emitted per window. The window is degraded and cannot produce AI credit.
- `head_move` has `old_head`, `new_head`, `branch_before`, `branch_after`, total `file_count`, at most 200 `paths`, `truncated`, and `commit_in_window`. It is based on one `git diff --name-only -z old new`; no file contents are read for this event. Dirty-scan changes in that window remain `external_change` with `reason: "head_moved"`.
- Window open/close records preserve their HEAD and branch state. A window is marked moved when either changes. A head move is a plain in-window commit when the new commit's parent list contains the old HEAD.

Unix store directories/files use mode `0700`/`0600`; Windows uses inherited per-user ACLs. Repo identity is SHA-256 of sorted first-parent root commit hash(es), independent of the worktree path. Shallow repositories and repositories without commits are skipped. Worktree identity is SHA-256 of the Git directory path.

## Limits

- Status still enumerates all untracked paths. More than 500 dirty paths deliberately produce a bulk summary instead of per-file evidence; 500 or fewer are still subject to the 1.5-second scan budget. The watchdog is armed at 1.4 seconds to leave scheduling margin under the hard two-second contract.
- Size/mtime equality skips reading a dirty path. An edit that preserves both values can therefore be missed until metadata changes; Git status is still the dirty-set source.
- A file error degrades only that path. Corrupt state/shadow JSON is treated as missing and starts fresh.
- State and shadow file syncs were retained because their measured combined sync phase was under 5 ms median in every benchmark case. Blob files are not synced.
- Linux and macOS compilation was not available here; Windows was measured. No network calls or Git configuration changes were made. Git AI checkpoint timing was executed only in temporary benchmark repositories with telemetry already disabled; no source was inspected.

## Benchmark

Run `cargo build -p prov-hook --bin prov-hook --example benchmark`, then `cargo run -p prov-hook --example benchmark`. Each case uses five repeats for in-process and real-binary Pre/Post events and plain Git status. Times are milliseconds, median / worst. The process-start value in the breakdown is estimated as paired binary time minus the in-process event time; it can reach zero under scheduling variance. Sync time is a subcomponent of writes. Git AI values are local one-file `checkpoint mock_ai` timings for comparison only.

| Case | Repo size | In-process Pre | In-process Post | Binary Pre | Binary Post | Plain status | Git AI checkpoint | Completed binary Pre/Post |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1k tracked clean | 1,000 tracked | 112.647 / 273.305 | 101.240 / 109.838 | 141.783 / 529.901 | 125.412 / 184.037 | 71.951 / 85.618 | 42.409 / 71.138 | 5/5, 5/5 |
| 1k tracked + 50 edits | 1,000 tracked | 127.298 / 191.441 | 219.417 / 494.097 | 129.434 / 216.496 | 230.466 / 451.826 | 65.033 / 100.096 | 22.135 / 27.039 | 5/5, 5/5 |
| 5k tracked clean | 5,000 tracked | 108.534 / 159.625 | 101.174 / 109.430 | 126.741 / 166.383 | 118.890 / 127.426 | 77.067 / 79.749 | 23.380 / 31.751 | 5/5, 5/5 |
| 5k tracked + 50 edits | 5,000 tracked | 129.257 / 160.241 | 223.488 / 525.385 | 162.171 / 262.639 | 249.755 / 514.310 | 92.456 / 99.029 | 23.167 / 30.395 | 5/5, 5/5 |
| a. 20k tracked clean | 20,000 tracked | 365.507 / 406.426 | 334.908 / 375.910 | 364.964 / 390.717 | 350.909 / 406.440 | 300.889 / 378.283 | 24.868 / 30.752 | 5/5, 5/5 |
| b. 20k tracked + 200 edits | 20,000 tracked | 401.341 / 468.432 | 553.376 / 1,178.420 | 402.176 / 952.778 | 520.432 / 1,215.828 | 351.159 / 429.110 | 29.257 / 90.377 | 5/5, 5/5 |
| c. 20k untracked | 20,000 untracked | 147.404 / 259.190 | 141.783 / 164.825 | 193.206 / 219.823 | 163.700 / 189.816 | 104.480 / 117.342 | 26.508 / 30.240 | 5/5 bulk summary |
| d. 20k tracked + 5k untracked + 200 edits | 25,000 files (5,200 dirty) | 462.552 / 533.426 | 401.768 / 426.861 | 452.412 / 532.288 | 435.271 / 495.046 | 396.014 / 512.048 | 31.325 / 80.834 | 5/5 bulk summary |

The target of 150 ms median per event through 5,000 tracked files is met for the clean 1k and 5k binary cases (Pre/Post: 141.783/125.412 ms and 126.741/118.890 ms). It is missed for the 1k and 5k 50-edit cases (129.434/230.466 ms and 162.171/249.755 ms). In the 5k edited case, status plus file reads, snapshots, writes, and event logging account for the added cost. Cases c and d finish as one bulk summary per window; they do not collect individual file hashes.

### Per-event median phase breakdown

Milliseconds, Pre / Post. `sync` is measured inside `writes`, not added a second time to totals. All sync medians were below 5 ms (largest was 3.916 ms), so state and shadow syncs remain enabled.

| Case | Process start | Discovery | Lock | Git status | Hash/snapshots | Writes | State/shadow sync | Event log | Remainder |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1k clean | 29.140 / 23.341 | 1.321 / 1.040 | 0.212 / 0.204 | 77.749 / 64.997 | 0.033 / 0.030 | 29.980 / 23.582 | 3.215 / 2.732 | 1.583 / 0.812 | 2.424 / 3.365 |
| 1k + 50 | 7.129 / 20.420 | 1.425 / 1.230 | 0.261 / 0.228 | 81.889 / 66.378 | 0.070 / 85.127 | 30.880 / 48.743 | 2.438 / 2.103 | 1.314 / 13.751 | 3.749 / 4.561 |
| 5k clean | 14.318 / 18.599 | 1.009 / 0.817 | 0.226 / 0.174 | 81.357 / 80.455 | 0.019 / 0.021 | 25.662 / 15.315 | 2.679 / 2.401 | 1.140 / 0.722 | 1.890 / 2.170 |
| 5k + 50 | 32.921 / 17.010 | 1.105 / 0.940 | 0.228 / 0.192 | 92.409 / 86.855 | 0.073 / 73.426 | 27.023 / 47.110 | 3.298 / 2.593 | 1.069 / 14.725 | 3.087 / 4.051 |
| a. 20k clean | 0.000 / 9.298 | 0.820 / 0.943 | 0.173 / 0.200 | 327.065 / 309.389 | 0.020 / 0.023 | 31.036 / 15.499 | 3.378 / 2.951 | 1.302 / 0.862 | 1.706 / 2.364 |
| b. 20k + 200 | 39.684 / 35.887 | 1.530 / 1.238 | 0.258 / 0.217 | 357.432 / 300.075 | 0.210 / 97.002 | 34.536 / 82.606 | 2.620 / 2.514 | 1.241 / 25.440 | 6.378 / 10.794 |
| c. 20k untracked | 21.125 / 27.146 | 0.950 / 0.782 | 0.154 / 0.154 | 87.824 / 91.005 | 23.121 / 24.149 | 23.129 / 19.685 | 2.638 / 2.884 | 10.037 / 0.722 | 1.725 / 2.204 |
| d. 20k + 5k untracked + 200 | 15.423 / 33.510 | 1.320 / 1.452 | 0.227 / 0.306 | 405.161 / 365.887 | 8.185 / 7.311 | 31.030 / 22.121 | 3.229 / 3.916 | 16.392 / 0.810 | 2.362 / 3.346 |

Git status dominates the 20k tracked cases. For edited tracked files, hashing/snapshots and event writes add time. For c and d, status enumeration dominates because file processing stops at the bulk cap. Repo discovery and locking stay small after caching.

The command-line fsmonitor experiment used `-c core.fsmonitor=true -c core.untrackedCache=true`, first call then four later calls, without writing Git configuration. At 5k the first/later median/worst times were 252.474 / 163.064 / 189.322 ms; at 20k they were 356.714 / 352.139 / 352.302 ms. There were no plain-status retries, no persistent daemon, and no observed `.git` metadata changes. An unavailable fsmonitor command in a disposable repo caused Git to print a spawn error but return success with plain-status output. The measured variant was slower than plain status on both sizes; network-drive behavior was not tested.

## Verification

Windows verification: `cargo fmt --package prov-capture --package prov-hook` completed; `cargo clippy --workspace --all-targets -- -D warnings` passed; `cargo test --workspace` passed **134 tests, 0 failed**. Contract tests cover dirty-only scans, excluded hash-only entries, tombstones, line ranges/deletions, bulk summaries, HEAD commit/checkout/pull movement, cache recreation, unmerged entries, lock markers, corrupt blobs, and Windows atomic replacement including failed-replacement temp cleanup. Linux/macOS compilation was unavailable in this environment.
