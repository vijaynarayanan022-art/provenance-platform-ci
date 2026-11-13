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

PR commits come from `git rev-list --no-merges <merge_base>..<head>`, capped at 500; `truncated` marks a larger range. The report retains the resolved requested base in `pr.base` and records the resolved merge base in `pr.merge_base`. If histories are unrelated, `merge_base` and `files_changed` are null, while the commit list falls back to the requested `base..head` range. Otherwise, `files_changed` is the distinct path count in the merge-base-to-head Git tree diff, including deletions. Missing reports are queued and processed once through the existing pending derive path, retaining its 50-commit/20-second cap, lock, attempt markers, and tombstones. Any remainder is underived. The report aggregates stored per-commit JSON and does not recompute line labels. Incomplete attribution metrics are `null`; percentages are rounded to one decimal, and empty eligible totals have null percentages. Evidence, coverage, tools, carried lines, and top files summarize only a complete range; a missing commit report or capped range makes unavailable aggregate fields null.

Counts are summed per commit, not deduplicated across the pull request: if a line is rewritten in two commits, it can count once in each commit. `tools` comes from stored AI-change events matched to each commit's changed paths and time window; rewrite source commits are included for carried attribution. The current capture source records `codex` as its agent tool.

To prepare `prs.json`, start with `gh pr list --state all --json number,title,author,state,url,baseRefOid,headRefOid`, then map `state` to `status` (`OPEN`/`MERGED`/`CLOSED` to `open`/`merged`/`closed`), `author.login` to `author`, and `baseRefOid`/`headRefOid` to `base`/`head`. If the PR commits are not present locally, first fetch them with `git fetch origin "refs/pull/*/head:refs/remotes/origin/pr/*"`.
## Round 8: coexistence and repo-less Git hooks

Codex setup now identifies its handler by the executable plus the sole `codex` argument, even when other command handlers share the same event group. Doctor checks for exactly one matching handler per event and remains read-only. Install updates only our command values or appends missing handler tables; uninstall removes only our tables and retains sibling handlers and hook-state tables byte-for-byte. Legacy marker blocks remain supported, and reversible `[features].hooks` metadata is preserved.

New Git hook blocks invoke `git-event` without `--repo`; the command resolves the worktree using Git's current-directory and environment context. Existing `--repo` invocations remain supported, and `install-git` upgrades old managed blocks without rewriting surrounding hook content. Windows verbatim `\\?\` prefixes are stripped from emitted and normalized paths.

## Round 10: VS Code Copilot agent hooks

`prov-hook vscode` is a second command adapter. It reads one VS Code agent-hook JSON object from stdin and parses the Claude-compatible common fields (`timestamp`, `hook_event_name`, `session_id`, `transcript_path`, and `cwd`), plus `tool_name`, `tool_input`, `tool_use_id`, and the PostToolUse `tool_response`. Transcript paths, tool responses, and file-write content are not used as evidence; file after-state is read from disk by the existing scanner. VS Code events are recorded with tool id `vscode-copilot`.

The command always exits zero and writes exactly `{}` to stdout, including malformed/empty input, caught panics, and watchdog expiry. This is required because a failing VS Code PreToolUse hook denies the tool call. It never writes a `permissionDecision`; diagnostics stay in the existing marker/inbox JSONL logs. The 1.4-second watchdog remains enabled. `SessionStart` is a no-op; `Stop` uses the Codex `turn_end` path.

`create_file` and `apply_patch` use the matching Pre/Post tool window and the existing before/after scanner, scoped to target paths. Apply-patch parsing extracts only `*** Update File:`, `*** Add File:`, and `*** Delete File:` paths; the patch body is not treated as evidence. Missing Pre windows are recorded as degraded `external_change` with `before_unavailable`, so derivation cannot make an exact AI ownership claim. Read-only tools (`file_search`, `get_errors`, `grep_search`, `list_dir`, `read_file`, `memory`, `vscode_askQuestions`, and `semantic_search`) are ignored. Unknown tool names create one `unsupported_tool` marker on PreToolUse and create no ownership event.

`run_in_terminal` follows the same generic dirty-working-tree window path as Codex shell/Bash hooks. It does not parse the command or infer changed files; VS Code's `tool_response` is ignored. A clean matching window with usable before/after snapshots can derive exact evidence and `ai_strong`; missing/corrupt snapshots, bulk scans, or other degraded conditions cannot become stronger than their evidence supports and remain unknown or degraded attribution. PR reports list `vscode-copilot` from stored AI-change events.

VS Code setup is user-level only: `prov-hook setup --tool vscode` writes or merges `<COPILOT_HOME or ~/.copilot>/hooks/prov-hook.json`, using `preToolUse`, `postToolUse`, and `agentStop`. `install --tool vscode`, `uninstall --tool vscode`, and `doctor --tool vscode` are also supported. Uninstall preserves other hooks and JSON fields; doctor checks file presence, JSON validity, and that all three command paths resolve to the current executable. The PowerShell command form and camelCase event keys are verified on Windows. The configured Bash command form is untested.

The apply-patch path parser is local to this adapter because the existing Codex capture path has no apply-patch-specific parser; attribution still comes only from snapshots. No Git AI source was read or reused. Linux/macOS behavior, Bash execution, edit tools beyond `create_file`/`apply_patch`, and remote/Codespaces environments are not verified.

## Round 11: hook lifecycle commands and summary

`prov-hook enable [--repo <path>] [--tools codex,vscode] [--dry-run]` coordinates the existing Codex install (including `[features].hooks = true`), VS Code setup, and Git post-commit/post-rewrite hook installation. `--tools` defaults to both adapters; Git hook installation is always attempted when a repository is available. If no repository contains the current directory, the Git step is `SKIPPED`. `disable` invokes the matching existing uninstall paths. Both commands honor `CODEX_HOME` and `COPILOT_HOME`, never modify `.github/hooks`, keep recorded capture data, and continue other steps after a failure. Each lifecycle step reports `PASS`, `FAIL`, or `SKIPPED` with its target paths. Enable prints Codex and VS Code doctor results after the install attempts; doctor failures are reported but only enable-step failures determine enable's exit code. Dry-run performs no writes and reports planned paths.

`prov-hook summary` defaults to the latest 10 derived non-merge commits on the current first-parent branch. It also accepts `--commit`, `--range`, or `--last`; missing commit reports are `not derived`, never zero-filled. Table columns are the stable ordered names in the command output, and percentages use the existing tenths-percent/coverage functions with `n/a` for zero eligible lines. JSON uses the additive `prov.summary/0.1` schema with `created_at` and the existing report `versions`. Range totals reuse PR-report commit selection and aggregate metrics, so the two commands agree for the same range; summary does not derive missing reports. Tool names come from `tools_for_commits`, including rewrite source commits.

`summary --file <path>` reads `prov.lines/0.1` attribution ranges and emits full category names, span, score, confidence band, primary basis, source tool, and operation. It does not invent file-line evidence; legacy sidecars without attribution ranges fall back to their existing nonblank added-line entries with no score or source metadata. Current line entries include `ai_direct` when a structured edit payload verifies against before/after snapshots and committed survival; excluded ranges can be present without scores.

## Round 12: per-line attribution metadata

`prov.lines/0.1` keeps its existing `files[].lines` entries and adds optional top-level `attributions` ranges. Ranges are contiguous per file and coalesced only when all metadata matches. The fields use camelCase: `startLine`, `endLine`, `category`, `score`, `confidenceBand`, `primaryBasis`, `supportingBasis`, `limitingBasis`, `source.adapterId`, `source.toolId`, `operation`, `algorithmProfile`, and `scoreProfile`. Legacy sidecars with no `attributions` field still load and remain unchanged when re-serialized without new ranges. Summary JSON exposes these as `file_ranges`; table detail rows use the same fields.

The score profile is `score-v1`; the range algorithm profile is `deterministic-v1`. Exact snapshot-derived lines use `EXACT_FRAGMENT_MATCH` with base score `0.92`. Duplicate ambiguity applies the specified `0.08` penalty and probable-category cap `0.79`. Time-cut events that cannot use the survival algorithm, and explicit degraded/incomplete sources, retain their existing penalties and category caps. Reconstructed windows use no primary basis, retain the probable score `0.60`, and carry `X_RECONSTRUCTED_WINDOW`. `MOVE_LINEAGE` is reserved for content-based rewrite carry-over. Mixed ranges retain the source evidence score where available and expose `CONFLICTING_EVIDENCE`; confidence bands use the standard score thresholds. Unknown and excluded ranges have null score and band `none`.

The following limitations have no corresponding basis code in Technical Design §5.2 and are therefore recorded with explicit `X_` prefixes until the spec owner decides whether to add standard codes: `X_TIME_CUT`, `X_OVERLAPPING_SESSIONS`, `X_RECONSTRUCTED_WINDOW`, and `X_DEGRADED`. No other `X_` codes are emitted.

## Round 16: direct attribution for structured edits

New change events record `structured_edit` as `true` for supported structured edits and `false` for scan-based tools, plus exact patch additions or a digest of full-file input needed to verify it against before/after snapshots. Legacy events without the marker are treated as scan-based. Codex `apply_patch` and VS Code `create_file`, `apply_patch`, `insert_edit_into_file`, and `replace_string_in_file` can produce `ai_direct` only when the payload additions match the captured before/after change exactly, the event is exact and non-overlapping/non-degraded, and the line survives unchanged to the commit. `create_file` payload content must match the full after snapshot; its added lines are then determined against the before snapshot. Direct accepted edits use `DIRECT_ACCEPTED_EDIT` at score `1.00`; patch operations use `DIRECT_AGENT_PATCH` at score `0.99`. Shell/Bash and `run_in_terminal` remain snapshot-derived `ai_strong`. Human edits retain the existing mixed attribution; ambiguous duplicate occurrences cannot be direct; overlap/time-cut/degraded/reconstructed evidence cannot become direct. Summary and line-sidecar counts include direct lines in deterministic percentages.

Rewrite carry-over synchronizes attribution range categories with the existing sidecar labels. Patch-ID carry uses `REBASE_PATCH_ID`; content carry uses `MOVE_LINEAGE` and remains capped at probable. Source metadata is copied from matching source ranges where present. Legacy sources without range metadata receive `INCOMPLETE_SOURCE`; existing category labels and report totals are unchanged by metadata generation.

FR-050 output uses the stable adapter tool id (`codex` or `vscode-copilot`) in `source.toolId`; the per-run `tool_use_id` is retained separately as `source.sessionRef`. Ranges include `endLineExclusive` alongside inclusive `endLine`. Summary JSON and table include `supportingBasis`, `limitingBasis`, `algorithmProfile`, and `scoreProfile`. Legacy ranges default to null primary basis and score, band `none`, and empty basis lists. Unknown and excluded ranges never carry a primary basis or score.

## Round 13: unobserved edit survival and mixed lines

When a file has a captured AI snapshot but no later observed external step, time-cut derivation aligns that snapshot to committed content. Exact AI-owned lines retain their category; if the file also has unmatched changed lines, exact survivors carry `X_UNOBSERVED_EDIT_IN_FILE`. Unmatched committed lines are unknown with `X_UNOBSERVED_EDIT`. Replaced lines are paired to AI-ancestry source lines once, in order; a material pair is `mixed` only when both trimmed lines meet the existing eight-character content threshold and lexical token overlap is at least `MIN_MIXED_OVERLAP = 0.5`. Tokens are identifiers, numbers, and operators; whitespace and delimiters are ignored. These mixed ranges use primary `CONFLICTING_EVIDENCE`, supporting `EXACT_FRAGMENT_MATCH`, and limiting `X_UNOBSERVED_EDIT`. Duplicate AI snapshot lines are probable under the existing ambiguity guard. Observed external steps and explicit degraded, overlap, reconstructed, missing-blob, and rewrite paths continue through their prior derivation paths.

The `prov.summary` and `prov.dashboard` `versions` objects add `line_survival: unobserved-mixed-v1` and `min_mixed_overlap: 0.5`. The per-range profile remains `deterministic-v1`; the new survival behavior is separately versioned to keep existing range profile consumers compatible. `X_UNOBSERVED_EDIT` and `X_UNOBSERVED_EDIT_IN_FILE` are spec gaps because §9.1 does not yet define standard limitation codes for unobserved post-snapshot edits.

## Round 18: duplicate-line ownership ambiguity fixed

Replay now carries candidate-owner sets for repeated identical lines touched by a diff. Occurrence order from the diff retains unambiguous matches; when repeated-line alignment has multiple plausible owners, replay unions their candidate lineages. A line receives an AI category only when every candidate has AI ancestry, and ambiguous AI-owned lines are capped at probable. Any candidate from base, external, or unknown ownership makes the line unknown. Fixed-seed fuzz and stress tests now run without ignores and enforce that lines without AI ancestry receive no AI category.

## Parent-sidecar fallback for unobserved edits

When the commit has no usable live events for added lines, derive compares the parent and committed file per diff hunk. It uses only parent `.lines.json` AI categories (`ai_direct`, `ai_strong`, `ai_probable`, or `mixed`) for removed lines, then pairs additions in order with a deterministic normalized bytewise Levenshtein similarity match. The minimum similarity is `0.5`; unpaired additions remain `unknown`. Paired additions are `mixed`, retain the removed line's source tool, and carry `X_UNOBSERVED_EDIT_IN_FILE`. The existing mixed scoring penalty applies. Parent-derived lines do not count as live coverage: a commit with no observed evidence reports coverage `none`; if other live evidence exists, fallback lines are removed from covered counts and the result is capped at `medium`.

For a new file (or any file without a parent sidecar), time-cut survival also compares the last AI after-snapshot to committed content per diff hunk. It gathers removed lines with AI ancestry from replay owners, then uses the same deterministic order-preserving similarity matcher and `0.5` threshold as the parent-sidecar fallback. A paired added line is `mixed`, retains the replaced line's source tool, and carries `X_UNOBSERVED_EDIT_IN_FILE`; unpaired additions remain `unknown`. Exact survivors keep their r15 category and `X_UNOBSERVED_EDIT_IN_FILE`. Snapshot-derived mixed lines are excluded from live coverage and trigger the same `medium` cap when the uncapped band would be `high`.

## Round 17: versioned exclusion classifier

Eligibility classification is implemented by `prov_capture::exclusion` with ID `exclusion-v1`, reported in summary/PR versions and on each excluded attribution range. Excluded additions receive category `excluded`, score `null`, and do not enter `E`, deterministic percentage, or linked percentage; the separate excluded count remains populated. Summary `--file` JSON and table output expose `exclusionClassifier` and `exclusionReason`.

Stable reason codes are `SENSITIVE_FILE`, `GENERATED_BYTECODE`, `DEPENDENCY_DIRECTORY`, `BUILD_OUTPUT_DIRECTORY`, `VENDORED_DIRECTORY`, `LOCKFILE`, `SOURCE_MAP`, `MINIFIED_FILE`, `BINARY_FILE`, `BINARY_CONTENT`, `GENERATED_HEADER`, and `FILE_SIZE_LIMIT`. Existing sensitive-path exclusions remain. Directory matching is case-insensitive and covers `node_modules`, `.venv`, `target`, `dist`, `build`, `vendor`, `vendors`, `third_party`, `third-party`, and `bower_components`. Lockfiles include `*.lock`, `package-lock.json`, `npm-shrinkwrap.json`, and the explicit package lock names in `exclusion.rs`. Source maps use `.map`; minified JavaScript/CSS uses `.min.js`, `.min.mjs`, `.min.cjs`, or `.min.css`. Binary extensions cover common image, font, archive, document, media, and compiled formats; NUL bytes in the first 8 KiB also classify content as binary. Generated headers are recognized when one of the first five physical lines contains `Code generated`, `@generated`, or `DO NOT EDIT` (case-insensitive). Non-binary path reasons take precedence over content reasons; for binary-extension paths, content classification takes precedence. NUL content preserves the existing `CONTENT_RESTRICTED` limiting basis and takes precedence over generated-header content.

FR-046/FR-063 require a versioned classifier and explicit policies but do not define its exact path sets, reason-code vocabulary, precedence, binary signature rules, or how many header lines to inspect. The choices above are this implementation's spec gaps and must be versioned if changed. Git submodule entries are still omitted as non-text gitlinks, so they contribute neither eligible nor excluded line counts; the docs require an explicit submodule policy but do not define a line-level count for them.

- r21: duplicate ambiguity among AI candidates from one adapter/tool/operation/session stays ai_strong (deviates from 04-Technical-Design section 9, which says probable or unknown; needs spec decision). Open: second create_file on an existing path records before_sha=None when no shadow baseline exists.

## Round 22: coverage degradation for unobserved edits

Technical Design §10.3 defines coverage as source, content, history, and analysis dimensions, reported with an overall band from a versioned profile (`complete`, `high`, `partial`, `low`, or `unknown`). It does not specify how unobserved edits affect that band or define numeric thresholds. Coverage profile `coverage-v2` therefore adds a conservative cap based on the share of eligible added lines carrying `X_UNOBSERVED_EDIT` or `X_UNOBSERVED_EDIT_IN_FILE`: at 5% the band is capped at `medium`, and at 20% it is capped at `low`. The thresholds are named constants in `prov-capture`; exact boundaries trigger the cap. Lines are counted once per file, and excluded lines are omitted. Five percent lets a small handful of affected lines lower a fully covered result one level, while twenty percent represents a substantial unobserved share and lowers it further. Commit reports retain `coverage_unobserved_lines`; summary and PR reporting recover that count from line sidecars for legacy reports. Both commands apply the same band rule. Coverage remains categorical; percentages continue to be shown alongside a coverage band.

## Round 23: squash file ranges and spec coverage bands

Summary `--file` resolves historical or deleted paths by canonicalizing their nearest existing ancestor and appending the missing path components. This keeps Windows canonical paths and requested paths in the same namespace when the file no longer exists in the current worktree. The command checks the selected commit reports for the requested path and returns an error if the file is outside the repo or absent from every selected commit. A regression test exercises a two-file `reset --soft` squash using the copied `prov.lines/0.1` sidecar fixture; it deletes the files from the test worktree before checking both range results and the missing-file error.

Coverage profile is now `coverage-v3`. Coverage strings are limited to the Technical Design §10.3 vocabulary: `complete`, `high`, `partial`, `low`, and `unknown`. Existing persisted `medium` is read as `partial`; legacy `none` is read as `low` when eligible lines exist and `unknown` when there are no eligible lines. Empty percentages remain `n/a`/null, so `unknown` describes the unavailable analysis without implying zero coverage. Confidence bands on attribution ranges are separate and retain `medium`.

The existing assertions updated for this change are: `prov_capture::commit::unobserved_edit_coverage_thresholds_and_r22_fixtures` expected `medium` at 5% and 19% unobserved lines (both now `partial`); the human-only coverage assertion in `prov-hook/tests/derive_cli.rs` (`none|low` now `low`); the three no-live-coverage assertions in that test file (`none` to `low`); the zero-eligible table assertion in `prov-hook/tests/commands_cli.rs` (`none` to `unknown`, keeping `n/a` percentages); and the PR version assertion (`coverage-v2` to `coverage-v3`). The coverage-version unit assertion was also bumped from `coverage-v2` to `coverage-v3`. Added checks cover zero totals and legacy `medium`/`none` normalization, including `none` with eligible lines.

PR report JSON now carries `file_ranges` from the same stored sidecars and range serializer as summary output; the range parity integration test compares both reports.

## Round 28: verified duplicate lines and boundary alignment

Verified structured edits now retain one-based line positions from their hash-checked snapshots through replay. Repeated text elsewhere in the same snapshot no longer makes those positionally verified additions unknown. Duplicate ambiguity remains active for candidates without verified positions, including human/external copies and independent AI evidence. `explain` includes competing AI event indices, tool-use IDs, sessions, and matching snapshot line locations when duplicate ambiguity fails; its candidate list is derived from the already filtered event sequence, so consumed and discarded events are omitted.

Capture line ranges now use the same Myers line alignment as attribution. For legacy evidence whose recorded range differs only by a one-line offset at a duplicated block boundary, explain accepts the alternate alignment only when both shifted boundary lines are repeated in the hash-verified after snapshot; direct positions use the recomputed range so the added-line set and attribution positions agree. This narrow compatibility rule is a spec deviation: the current design does not define how to reconcile historically recorded line ranges when identical boundary lines admit multiple alignments. It should be reviewed by the spec owner.

The existing table assertion in `prov-hook/tests/commands_cli.rs::summary_marks_missing_reports_and_zero_eligible_percentages` was also changed from `none` to `unknown`, keeping the `n/a` percentage expectation.

The exact PR-report key assertion in `prov-hook/tests/pr_report_cli.rs::two_commit_report_sums_stored_reports_and_out_matches_stdout` now includes the additive `file_ranges` field.

## Round 30: reset-away identical patch and explain parity

Patch-ID rewrite carry-over is now monotonic: it may promote a weaker target label from a matching source, but it cannot replace a stronger target label with a weaker one. This closes a case where a fresh, exact shell AI event was first derived as `ai_strong`, then overwritten by `unknown` labels from an identical patch that had been reset out of the target commit's ancestry. Existing candidate ancestry, overlap, and duplicate ambiguity rules for discarded or copied text remain in force.

`explain` now reports category and score from its current line attribution decision, rather than mixing a stored post-rewrite category with a recomputed score. Unknown lines add a `final_attribution` check that identifies the committed-alignment step and source step that failed to retain AI ownership. The r30 fixture carries the real bundle and store with `* -text`; tests cover summary and explain output, reset without repeated content, and repeated content without reset. This intentionally narrows the prior patch-ID carry behavior that could downgrade labels, resolving an unspecified conflict between exact patch reuse and stronger current evidence.

## Round 32: VS Code Copilot CLI-backed runtime hooks

VS Code setup now writes the VS Code-compatible PascalCase hook events `PreToolUse`, `PostToolUse`, and `Stop`, with version 1, `command` and `powershell` commands, and a five-second timeout. Install migrates this adapter's prior camelCase entries while preserving foreign entries; uninstall removes both generations. Doctor fails with an explanation when only the legacy camelCase entries are present because the Copilot CLI-backed VS Code runtime selects its payload format from the event-name casing.

The dispatcher compares known tool names case-insensitively. Evidence-backed `Edit` string payloads use the existing patch verifier and can become `ai_direct` only when the patch matches captured snapshots. Malformed `Edit` payloads retain snapshot-diff attribution capped at `ai_strong`. Evidence-backed `Bash` events use terminal-style snapshot diffs. Other non-read-only names still produce `unsupported_tool`; no unobserved `Write`, `MultiEdit`, `NotebookEdit`, or similar tool is granted direct attribution. Existing r26 names remain supported.

The `r32_copilot_runtime` test fixture includes sanitized raw probe records, the captured store, and its work bundle, with `* -text`. Integration coverage replays the observed file create, structured update, terminal create, and delete through summary and explain. Deleted files have no surviving added lines, and `explain` currently rejects deleted-only paths even though the commit summary records the deletion. The integration test therefore explains the terminal-created file and verifies the combined create/delete commit summary; direct explain output for a deletion is a current command limitation, not a new attribution claim.

The `vscode_cli` test helper serializes hook subprocess calls to keep default-parallel test runs from starving the production two-second hook budget; the Cargo test runner remains in its default parallel mode. This avoids spurious `time_cap` markers in otherwise unrelated adapter tests.

## Round 33

Malformed or unrecognized Codex and VS Code hook payloads remain fail-open with their existing host-visible output. The capture store records one `unrecognized_payload` marker per session, event-name spelling, and sorted top-level key set; marker contents exclude payload values and paths. Both Codex and VS Code doctor commands warn about markers from the last seven days without changing their exit status. The 1.4-second watchdog is unchanged.

## Round 34

When an eligible unknown added line replaces a nonblank parent line, derivation follows Git blame to the line's writing commit and consults only that origin's stored derivation. If its stored category is `ai_direct`, `ai_strong`, `ai_probable`, or `mixed`, and the existing token overlap is at least `min_mixed_overlap` (0.5), the edit is `mixed` with `HUMAN_EDIT_OF_PRIOR_AI_LINE`, score 0.84, and origin commit, line, category, and overlap in the attribution. Insertions, blank-line replacements, rewrites below threshold, missing or stale origin reports, and lookup errors stay unknown. The one-blame-per-file lookup has a two-second cap; it never derives origins on demand. Carried lines retain `X_UNOBSERVED_EDIT` and count toward the existing coverage-v3 caps. The line-survival version changes to `unobserved-mixed-cross-commit-v2`; an origin sidecar without this version is stale for this rule and must be re-derived locally.

This is a **proposed spec change pending sign-off** on three decisions: use `mixed` for the human edit of prior AI text; follow the line to its origin without a fixed ancestry depth; and count carried lines toward unobserved-edit coverage. Known limit: the origin commit must already have a current stored derivation in the local provenance store.
