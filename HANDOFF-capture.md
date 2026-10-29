# Capture R1 handoff

## What exists

- `prov-capture` implements the Codex hook payload parser, read-only Git discovery, working-tree scanner, snapshot store, hash-based change events, line ranges, deduplication, overlap detection, exclusive per-repository locking, and the 1.9-second process watchdog.
- `prov-hook` is a small `codex` binary. It reads one JSON value from stdin, emits no output, and returns success for malformed input, unsupported invocations, and contained errors.
- Workspace membership is updated in the root `Cargo.toml`; the lockfile contains the resolved dependencies. No existing crate source was edited.

## On-disk formats

The store root is `PROV_HOME` when set. Otherwise it is `%LOCALAPPDATA%\prov` on Windows, `~/Library/Application Support/prov` on macOS, or `$XDG_DATA_HOME/prov` (defaulting to `~/.local/share/prov`) on other Unix systems.

- `repos/<repo_id>/blobs/<sha256>` stores the bytes of a captured text file. Files are written through a same-directory temporary file, flushed, synced, then renamed.
- `repos/<repo_id>/shadow/<worktree_id>.json` is a JSON object with a `files` map. Each forward-slash path maps to `size`, `mtime_ns`, and `sha256`.
- `repos/<repo_id>/events/<session_id>.jsonl` is append-only JSONL. Each complete line has `schema_version: "prov.event/0.1"`, `event`, and a nanosecond Unix timestamp. Event kinds include `external_change`, `window_open`, `ai_change`, `window_close`, `turn_end`, `scan_incomplete`, and degraded/lock markers.
- `inbox.jsonl` in the store root receives lock timeout and watchdog markers. JSONL readers ignore an unterminated final line.

Directories are created with user-only mode `0700` on Unix. Windows uses the current user's inherited local-data ACLs.

`repo_id` is the SHA-256 of the sorted reachable root commit hashes joined by newline. `worktree_id` is the SHA-256 of the absolute Git directory path for that worktree.

## Limits and behavior

- A repository with no reachable root commit is skipped. `repo_id` is the SHA-256 of sorted root commit hashes, so repositories with no commits have no stable identity.
- The scanner sees paths returned by `git ls-files -z --cached --others --exclude-standard`. It snapshots only regular text files no larger than 1 MiB, skipping NUL-containing content and the constant exclusion patterns in `prov-capture`.
- A file is re-read only if its size or recorded modification time changes. Edits that preserve both metadata values can be missed. Symlinks are skipped.
- Pre-tool observations are recorded as `external_change`; post-tool observations are `ai_change` with command SHA-256 only. The command text and transcript fields are not stored or logged.
- A scan stops after 1.5 seconds and marks the window degraded. The process watchdog exits at about 1.9 seconds and attempts to append a degraded marker. A hard process termination can leave only the events already appended; readers must ignore the torn final line.
- Lock acquisition currently gives up after 650 ms (below the one-second maximum) to leave room for Git startup and inbox recording within the hook's overall deadline.
- File-change events are append-only. An overlapping window is detected from recorded window intervals; if a hook process is killed before its close event, that window remains open in the log and later windows may conservatively be marked overlapping.
- The benchmark example uses a single committed README plus 20,000 untracked small files, then changes 200 files. This Windows run printed **1,892 ms cold** and **1,962 ms warm**. The benchmark calls the capture library directly so it can report scan work that may outlast the executable's 1.9-second watchdog.

## Verification

The Windows workspace test suite passes 114 tests, and strict Clippy passes. `cargo check` passes for `x86_64-unknown-linux-gnu` and `aarch64-apple-darwin`; tests were not run on those operating systems. The benchmark example prints cold and warm scan times; those measurements should be rerun on the target deployment machine for representative storage performance. A real full-disk condition was not induced. Windows directory privacy relies on the ACL inherited from the per-user local-data directory.
