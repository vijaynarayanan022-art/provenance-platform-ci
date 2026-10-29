# Capture R1b handoff

## What exists

`prov-hook` is a silent `codex` hook executable. It reads JSON from stdin, hashes command text without retaining it, catches panics, and exits successfully. `prov-capture` discovers the Git root and git directory by walking upward from `cwd` and reading `.git` and `HEAD` files directly. It caches repository identity by git directory, and runs one `git status` process for normal events (a second `cat-file` process is used when an event needs a HEAD baseline). It scans only Git's porcelain-v2 dirty set, serializes work under a per-repository file lock, and records external/AI changes, turn windows, overlap, deduplication, head movement, and degraded markers.

For a tracked dirty file, the before content is read from the index's HEAD blob; an untracked file has an empty before image. Oversized, binary, symlink, and excluded paths are never snapshotted. Excluded text files still produce hashes and line ranges in memory. Individual file errors are attached to that file and do not stop later dirty paths. Clean files have no shadow entries.

The hook does not read transcript fields, write inside the worktree, or invoke mutating Git operations. Git calls use `GIT_OPTIONAL_LOCKS=0`; status and file-list output are NUL delimited. Known-repository lock, watchdog, and skip markers use `repos/<repo_id>/markers.jsonl`; unknown-repository markers use `inbox.jsonl`.

## On-disk formats

The store root is `PROV_HOME` when set. Otherwise it is `%LOCALAPPDATA%\\prov` on Windows, `~/Library/Application Support/prov` on macOS, or `$XDG_DATA_HOME/prov` (default `~/.local/share/prov`) on other Unix systems.

- `repo-cache/<sha256-gitdir>.json` caches `{ "repo_id": "..." }`, keyed by the Git directory path.
- `repos/<repo_id>/blobs/<sha256>` stores content-addressed text snapshots written through a same-directory temporary file, flush, sync, and atomic rename/replacement.
- `repos/<repo_id>/events/<session_id>.jsonl` contains append-only `prov.event/0.1` records. Appends use `write_all`; batched file-change events are serialized in memory and appended together. Before appending, an unterminated tail is separated by a newline so the next event remains readable.
- `repos/<repo_id>/markers.jsonl` contains known-repository `lock_timeout`, `time_cap`, and repository-skip markers. `inbox.jsonl` is used when no repository id is known.
- `repos/<repo_id>/shadow/<sha256-gitdir>.json` stores only dirty-file observations under `files`, keyed by forward-slash path. Each entry has `size`, `mtime_ns`, and `sha256`. Clean files are absent.
- `repos/<repo_id>/state/repo.json` contains active windows (`session_id`, `tool_use_id`, open timestamp, HEAD/branch, overlap), at most 1,000 recent dedupe keys, and `scan_incomplete_pending`. Windows and keys older than one hour expire.

Unix directories/files use mode `0700`/`0600`; Windows uses inherited per-user ACLs. Repository identity is SHA-256 of the root commit hash of HEAD's first-parent chain, cached by Git directory. Shallow repositories are skipped rather than treating the shallow boundary as a root. Repositories with no commits are skipped without recording because no root identity exists. Worktree identity is SHA-256 of the Git directory path; moving a repository preserves `repo_id` but changes this worktree key.

## Limits

- Git status must enumerate untracked paths; very large dirty/untracked sets can exceed the 1.5-second scan budget. A pass then writes `scan_incomplete`, marks the window degraded, and carries `after_incomplete=true` onto the next window's external changes.
- The hook watchdog is armed for 1.4 seconds to leave scheduling margin under the hard two-second contract, and attempts a `time_cap` marker. A hard process exit can prevent state updates; complete JSONL lines remain readable and a torn final line is ignored.
- `size` and `mtime_ns` are stored for dirty file observations. Git status is the dirty-set source; it can report content changes that preserve metadata, but a file changed and restored before the hook runs is inherently not observable.
- A Git AI baseline below is only a timing reference for `git-ai checkpoint mock_ai` in disposable local repositories. It is not an equivalence or quality comparison.
- Linux and macOS compilation was not available in this environment. Windows was measured. Network-drive behavior was not tested. Git's fsmonitor daemon did not remain active in this environment; commands succeeded and showed no `.git` metadata changes. An unavailable fsmonitor command was checked in a disposable repo: Git reported the spawn error but returned success and fell back internally to plain status.
- No network requests were made. Benchmark setup and the Git AI baseline ran only in disposable temporary repositories. No Git configuration was changed; fsmonitor settings were passed on the command line.

## Benchmark

Run `cargo build -p prov-hook --bin prov-hook --example benchmark` then `cargo run -p prov-hook --example benchmark`. The executable runs five repeats per case and prints median/worst milliseconds. The binary timing includes process startup; phase breakdowns are medians from paired in-process hook calls, with process-start estimated as paired binary wall time minus in-process total. Thus process-start is an estimate and may reach zero under scheduling variance. `Git status` is a separate plain-status baseline on the same fixture. `Git AI` is five local calls to `C:\\dev\\git-ai-bin\\git-ai.exe checkpoint mock_ai <one path>` with telemetry disabled; no source was inspected.

| Case | Repo size | In-process Pre median/worst ms | In-process Post median/worst ms | Hook binary Pre median/worst ms | Hook binary Post median/worst ms | Plain status median/worst ms | Git AI one-file median/worst ms | Scan complete binary Pre/Post |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1k tracked clean | 1,000 tracked | 77.476 / 166.827 | 115.266 / 156.089 | 95.887 / 357.689 | 89.454 / 93.959 | 50.563 / 51.133 | 23.030 / 48.725 | 5/5, 5/5 |
| 1k tracked + 50 edits | 1,000 tracked | 85.134 / 140.918 | 160.372 / 422.459 | 100.739 / 132.398 | 185.746 / 384.354 | 55.950 / 59.801 | 21.018 / 23.204 | 5/5, 5/5 |
| 5k tracked clean | 5,000 tracked | 97.065 / 142.243 | 88.836 / 92.908 | 118.119 / 147.800 | 106.099 / 115.100 | 71.205 / 76.175 | 20.783 / 23.599 | 5/5, 5/5 |
| 5k tracked + 50 edits | 5,000 tracked | 113.207 / 142.613 | 170.550 / 422.221 | 127.675 / 170.656 | 211.067 / 417.428 | 79.594 / 88.096 | 21.060 / 23.709 | 5/5, 5/5 |
| a. 20k tracked clean | 20,000 tracked | 342.147 / 361.315 | 302.304 / 339.023 | 334.007 / 397.736 | 349.985 / 365.772 | 281.858 / 310.796 | 20.587 / 26.533 | 5/5, 5/5 |
| b. 20k tracked + 200 edits | 20,000 tracked | 330.642 / 401.908 | 466.854 / 1,275.522 | 344.768 / 422.332 | 450.158 / 1,275.522 | 287.191 / 314.599 | 29.626 / 38.014 | 5/5, 5/5 |
| c. 20k untracked | 20,000 untracked | 1,604.543 / 1,663.445 | 1,601.703 / 1,649.921 | 1,624.519 / 1,649.921 | 1,615.747 / 1,620.102 | 91.523 / 181.083 | 30.094 / 66.037 | 0/5, 0/5 |
| d. 20k tracked + 5k untracked + 200 edits | 25,000 dirty paths | 1,593.270 / 1,616.004 | 1,613.257 / 1,655.022 | 1,626.000 / 1,741.821 | 1,644.480 / 1,775.409 | 392.935 / 632.198 | 21.948 / 31.273 | 0/5, 0/5 |

The target of under 300 ms median hook time for 20k cases a and b was missed: even clean binary medians were 334 ms Pre and 350 ms Post. Case b completed all scans, but Post median was 450 ms. Cases c and d exceeded the 1.5-second scan budget in every repeat, and each stayed below the 1.9-second process cap in this run. The hook performs bounded work and reports incomplete results rather than claiming completion.

### Per-event median phase breakdown

All phase values are milliseconds. `start` is the estimated process-start component described above; the other phases are instrumented in-process medians. The phases are Pre / Post.

| Case | Process start | Repo discovery | Lock | Git status | Hashing and snapshots | Writes | Event log | Uninstrumented remainder |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1k clean | 20.449 / 19.365 | 0.770 / 0.749 | 0.202 / 0.157 | 52.688 / 49.348 | 0.018 / 0.014 | 21.235 / 15.437 | 1.046 / 0.577 | 1.922 / 1.828 |
| 1k + 50 | 16.534 / 16.014 | 0.889 / 0.849 | 0.164 / 0.168 | 56.341 / 49.949 | 0.058 / 61.471 | 21.866 / 34.668 | 0.998 / 11.538 | 2.211 / 3.630 |
| 5k clean | 17.223 / 18.858 | 0.943 / 0.652 | 0.165 / 0.144 | 70.648 / 70.597 | 0.016 / 0.015 | 21.978 / 14.985 | 0.958 / 0.538 | 1.359 / 1.678 |
| 5k + 50 | 18.715 / 29.246 | 0.819 / 0.867 | 0.170 / 0.166 | 79.639 / 73.458 | 0.056 / 62.255 | 23.437 / 33.921 | 1.221 / 11.070 | 2.519 / 3.262 |
| a. 20k clean | 16.163 / 33.576 | 1.349 / 1.050 | 0.191 / 0.189 | 283.128 / 273.207 | 0.016 / 0.015 | 29.726 / 21.443 | 1.420 / 0.909 | 1.441 / 2.233 |
| b. 20k + 200 | 28.697 / 0.561 | 1.077 / 1.418 | 0.175 / 0.320 | 299.068 / 288.262 | 0.147 / 96.451 | 29.631 / 51.211 | 1.144 / 18.523 | 5.919 / 9.819 |
| c. 20k untracked | 1.550 / 19.296 | 1.175 / 0.887 | 0.178 / 0.185 | 90.574 / 90.552 | 370.409 / 429.351 | 1,113.348 / 1,034.718 | 25.742 / 22.374 | 11.792 / 16.337 |
| d. 20k + 5k untracked + 200 | 11.823 / 12.655 | 0.965 / 0.991 | 0.222 / 0.226 | 306.166 / 304.135 | 274.363 / 455.931 | 946.592 / 812.032 | 21.715 / 28.289 | 14.395 / 27.658 |

Git status dominates the clean tracked cases and is already near the hook's total. For changed tracked files, reading, hashing, snapshots, and append work add time. The untracked-heavy cases spend most of their budget writing snapshots and state/events before the scan deadline. Repository discovery and locking are small after the cache is warm. I recommend keeping the plain status path as the default: the command-line fsmonitor experiment was slower on both sizes here, and the daemon reported it was not watching the repos.

### Fsmonitor experiment

The benchmark passed `-c core.fsmonitor=true -c core.untrackedCache=true` only on the status command. It ran one first call and four later calls on clean tracked repositories. There were zero plain-status retries because Git returned success. It reported `error: daemon terminated`, the daemon was inactive, and the measured `.git` metadata walk found zero changed or removed paths.

| Tracked files | First call ms | Later median/worst ms | Plain retries | `.git` changes/removals | Daemon active |
|---:|---:|---:|---:|---:|---:|
| 5,000 | 143.709 | 124.878 / 131.851 | 0 | 0 / 0 | No |
| 20,000 | 329.661 | 322.954 / 327.888 | 0 | 0 / 0 | No |

This machine's command-line variant was slower than its plain-status medians (71.205 ms at 5k and 281.858 ms at 20k) and provided no measured metadata changes. The benchmark retries plain status if the fsmonitor status call returns failure. With an unavailable fsmonitor command, Git itself emitted a spawn error, returned success, and returned plain-status output. Network-drive behavior is unverified.

## Verification

Windows benchmark and workspace verification results are recorded with the final task response. The benchmark's real-binary measurements include a new process for each Pre/Post event. Five repeats were used in all cases and fsmonitor/Git AI baseline runs. The Git AI command ran in the disposable fixtures only, with its telemetry-off configuration already in effect; no telemetry or version-check settings were changed during the run.
