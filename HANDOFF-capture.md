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
- `repos/<repo_id>/shadow/<sha256-gitdir>.json` stores dirty-file entries with `size`, `mtime_ns`, `sha256`, `excluded`, and `deleted`. Excluded entries never have blobs. Deletion tombstones retain the previous hash and have `deleted: true` while the deletion remains dirty. Clean paths are removed from shadow state. In bulk mode, the scan writes hash-only shadow entries for dirty files without persisting the blob, so any later derivation must treat that prior state as content unknown rather than empty.

Round 2a tightened `BeforeContent` selection: a porcelain-v2 rename target or an unmerged path is `Unavailable`, while only truly untracked or HEAD-absent paths remain `KnownEmpty`. Bulk batching is still summarized by a single `bulk_change` event with hash-only shadow state; any oversized HEAD blob is skipped and degraded before `ai_change` is emitted.
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

Windows verification: `cargo fmt --package prov-capture --package prov-hook` completed; `cargo clippy --workspace --all-targets -- -D warnings` passed; `cargo test --workspace` passed **135 tests, 0 failed**. Contract tests cover dirty-only scans, excluded hash-only entries, tombstones, line ranges/deletions, bulk summaries, HEAD commit/checkout/pull movement, cache recreation, unmerged entries, lock markers, corrupt blobs, and Windows atomic replacement including failed-replacement temp cleanup. Linux/macOS compilation was unavailable in this environment.

`prov_capture::derive` provides a pure, deterministic byte-line replay API that carries ownership through capture snapshots and labels added committed lines as AI strong/probable, mixed, or unknown. It uses `similar` for line alignment, treats post-evidence edits as unknown, and conservatively downgrades duplicate committed text. Its unit suite includes fixed-seed human/AI edit fuzzing in unique-text and duplicate-heavy modes.

Round 2b-2 adds `prov_capture::commit` to derive deterministic `prov.commit/0.1` JSON from commit trees and stored capture events. It replays verified snapshot blobs, cuts evidence at the parent and committed content, and reconstructs commits recorded inside an AI window as degraded AI evidence. Merge commits are recorded as skipped; deleted paths are omitted, while excluded, binary, and oversized file additions are counted outside `E`. Results are sorted and atomically stored under the external capture store, without writing into the analyzed repository. `prov-hook derive` supports one commit or a revision range, and `prov-hook show` prints a summary or stored JSON. Integration tests exercise exact, mixed, reconstructed, missing-blob, skipped-path, merge, repeatability, and worktree-status behavior.

Oversized blobs are excluded using `cat-file --batch-check` sizes and are never passed to `cat-file --batch`. Their excluded addition count comes from `git diff-tree --numstat`, so it may include blank lines.

Commit derivation normalizes CRLF to LF for parent draining, exact snapshot matching, and line ownership replay; lone carriage returns remain unchanged. Binary, size, excluded-path, and added-line accounting continue to use original blob bytes.

Round 3a adds `prov-hook install`, `uninstall`, and `doctor`. Install manages only the marked hook block in `config.toml`, using the Codex PreToolUse/PostToolUse/Stop table layout and a quoted executable command. Changes are backed up to timestamped sibling files; dry-runs emit unified diffs without writes, and repeated installs are byte-idempotent. Uninstall removes only the marked block. Setup never creates or edits Codex hook trust hashes/state; doctor reports unapproved hooks as a warning to open Codex and approve them. Doctor checks the executable, config, hooks, store permissions, Git, and optional cached repo identity/event history without writing probes or changing the analyzed worktree.

## Round 4a

After a `head_move` event is successfully appended, `prov-hook codex` starts a detached one-shot `prov-hook derive --pending --repo <root>` process; it never waits for derivation, and `PROV_NO_AUTODERIVE=1` disables this trigger for tests. Pending derivation uses `repos/<repo_id>/derive.lock` (fresh for 120 seconds) and per-commit `.attempt` markers (fresh for 60 seconds), processes oldest head moves first, and caps each run at 50 commits or 20 seconds. Stale locks/markers are retried; merge commits already have skip records. Each report has an atomic `commits/<sha>.lines.json` sidecar with schema `prov.lines/0.1`, EOL-normalized line hashes only, and `s`/`p`/`m`/`u` labels; commit JSON adds stable `patch_id` or null. Doctor reports pending commits read-only. Codex's background server does not inherit a shell's `PROV_HOME`; production must not set it.

## Round 4b

Deriving a rewritten commit performs a bounded post-step using only this repository's stored commit reports and hash-only line sidecars. It first stats sidecar filenames and modified times, keeps the newest 200 within the 14-day candidate window plus a one-day margin, then batches Git object-existence and committer-time checks before applying the exact non-ancestor/time/14-day filters and retaining the newest 50; only those final sidecars are parsed. Exact non-null `patch_id` matches carry labels by ordered `(path, hash)` multiset pairing. Without a patch pair, a content source is used only when at least 50% of its eligible added-line multiset appears in the target; matching also requires the target line's trimmed text to be at least eight characters and caps inherited labels at `ai_probable`. A source line is used at most once per path; conflicting source labels resolve to the weakest (`unknown < mixed < ai_probable < ai_strong`). Commit JSON adds `rewrite.method`, sorted `rewrite.sources`, and `rewrite.carried_lines`; zero carried lines retain method `none`, empty sources, and the original files unchanged. Files with carried labels count as covered when recomputing coverage, without changing their evidence value. Totals and the sidecar are recomputed after carry-over. Carry-over stops without changing the normal report if it exceeds five seconds or 200,000 hash entries. It does not cover interactive rebases that split commits, rewrites older than 14 days or outside the newest 50 sources, or cross-repository moves.

## Round 7: git-side post-commit and post-rewrite trigger

This round adds the git-side trigger path that feeds the existing pending-queue and detached auto-derive flow in the repo without writing into the analyzed worktree.

### Commands
- `prov-hook git-event post-commit --repo <path>`: resolves the repo root, reads the current `HEAD`, and enqueues that commit hash if the repo already has a capture store/history.
- `prov-hook git-event post-rewrite --repo <path>`: reads stdin as newline-delimited `<old> <new>` pairs, keeps only the newest SHA per line, caps the queue at 1000 entries, and enqueues the new commit SHA values.
- `prov-hook install-git --repo <path>`: installs hooks into the effective hooks directory (absolute or relative `core.hooksPath`, or `git rev-parse --git-path hooks`) for `post-commit` and `post-rewrite`.
- `prov-hook uninstall-git --repo <path>`: removes only the managed block and restores the original hook bytes exactly.
- `prov-hook doctor --repo <path>`: prints the git-hooks status line without writing anything to disk.

### Queue semantics
The queue reuses the existing pending-derive mechanism in `prov-capture`. Once a hook adds one or more git SHAs, the hook immediately spawns the detached `prov-hook derive --pending --repo <root>` command, honoring `PROV_NO_AUTODERIVE=1` as a test/debug bypass. If the repo has no capture store/history, the hook exits silently and creates nothing.

### Hook behavior
- `post-commit` runs in the background and redirects stdin from `/dev/null`.
- `post-rewrite` captures stdin into a temporary file, launches the git-event processor in the background, then replays stdin to the remainder of the original script so the original hook continues to receive the rewrite payload exactly as it expects.
- Existing shell hooks are preserved outside a marked block. Non-shell hooks are left untouched and reported as `unsupported` without being modified.
- Uninstall removes only the managed block; if the file was created for us and nothing else remains, the file is deleted.

### Known limits
- The trigger is intentionally quiet and non-blocking. It never fails the Git operation if the capture store is absent.
- The queue is best-effort and limited to a short, bounded set of commit SHAs; the existing pending derive loop remains the authority for replay and backpressure.
- The git-side trigger is not a replacement for the capture-store record itself; it only feeds the existing deterministic derive pipeline.

## Round 9: per-PR attribution report

`prov-hook pr-report --repo <path> --base <rev> --head <rev>` emits a single `prov.pr-report/0.1` JSON object. Add optional `--pr`, `--title`, `--author`, `--status`, and `--url` metadata. `--prs-file <prs.json>` emits a `prov.dashboard/0.1` document with PR entries in file order; `--out <file>` writes the same JSON bytes that otherwise go to stdout. `--no-derive` reports missing commit reports as underived without failing.

PR commits come from `git rev-list --no-merges <base>..<head>`, capped at 500; `truncated` marks a larger range. Missing reports are queued and processed once through the existing pending derive path, retaining its 50-commit/20-second cap, lock, attempt markers, and tombstones. Any remainder is underived. The report aggregates stored per-commit JSON and does not recompute line labels. Incomplete attribution metrics are `null`; percentages are rounded to one decimal, and empty eligible totals have null percentages. `files_changed` is the distinct path count in the base-to-head Git tree diff, including deletions. Evidence, coverage, tools, carried lines, and top files summarize only a complete range; a missing commit report or capped range makes unavailable aggregate fields null.

Counts are summed per commit, not deduplicated across the pull request: if a line is rewritten in two commits, it can count once in each commit. `tools` comes from stored AI-change events matched to each commit's changed paths and time window; rewrite source commits are included for carried attribution. The current capture source records `codex` as its agent tool.

To prepare `prs.json`, start with `gh pr list --state all --json number,title,author,state,url,baseRefOid,headRefOid`, then map `state` to `status` (`OPEN`/`MERGED`/`CLOSED` to `open`/`merged`/`closed`), `author.login` to `author`, and `baseRefOid`/`headRefOid` to `base`/`head`. If the PR commits are not present locally, first fetch them with `git fetch origin "refs/pull/*/head:refs/remotes/origin/pr/*"`.
