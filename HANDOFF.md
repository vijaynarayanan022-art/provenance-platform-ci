# Handoff: Code Provenance Platform (state as of 2026-10-21)

Read this first if you are picking the project up in a new session or with a new person.

## Goal
Build the company's vendor-neutral Code Provenance Platform per the 8-file design doc set
(README, 01 BRD ... 07 Delivery). Evidence-based attribution of AI-generated code to lines/commits/PRs.
Key rules: categories `ai_direct/ai_strong/ai_probable/mixed/unknown/excluded`; **unknown is never human**;
PR metric over eligible added lines only:
`AI_deterministic% = 100(D+S)/E`, `AI_linked% = 100(D+S+P+M)/E`, `E = D+S+P+M+U` (excluded never in E).

## Decision taken
Do NOT fork Git AI. Run it unmodified (hardened config) as the **capture layer**; read its Git notes through the
adapter in this repo (`crates/prov-gitai`) into our own engine (`crates/prov-core`). Later: Agent Trace as an
export format; borrow signing/policy ideas from agentdiff. Git AI covers roughly one third of the whole project, mostly capture.

## What is installed on Vijay's PC (undo steps included)
- Git AI source: `C:\Users\Hp\OneDrive\Desktop\Provenance Platform\git-ai` (debug build). Move out of OneDrive when convenient.
- Stable binary used by every hook: `C:\dev\git-ai-bin\git-ai.exe` (copy of the debug build; path has no spaces on purpose).
- `install-hooks` was applied: Claude Code, Codex, Cursor, Copilot, OpenCode, Gemini hooks; VS Code + Cursor extension `git-ai.git-ai-vscode`;
  global git config `trace2.eventtarget` (named pipe) and `trace2.eventnesting=0`.
- Backups: `C:\Users\Hp\git-ai-backup\codex-config.toml`, `vscode-settings.json`; `~\.gitconfig.backup`.
- Hardened config: `telemetry_oss off`, `prompt_storage local`, `disable_version_checks true`, `disable_auto_updates true`,
  feature flags `daemon_log_upload`, `token_usage_metrics`, `rewrite_metrics_events` = false. `api_base_url` still usegitai.com (unconfirmed whether anything is sent without login).
- Debug builds do NOT auto-start the daemon. After every reboot: `C:\dev\git-ai-bin\git-ai.exe bg start`. Check: `bg status`.
- Before replacing the exe: `git-ai.exe bg shutdown --hard`.
- **Undo everything:** `C:\dev\git-ai-bin\git-ai.exe uninstall-hooks`, then restore backups if needed.
- Codex must be fully restarted to pick up hooks (it was running during install).
- Lab repos: `C:\dev\lab\test-repo`, `C:\dev\lab\test-repo2`, outputs in `C:\dev\lab\samples`.

## Lab findings (real output, Git AI development:1.7.6, `mock_ai` test preset)
Scenario: 3 human lines committed; then 4 AI lines + AI checkpoint; then 2 human lines; commit.
1. **No human marker** (test 1): note credited lines 8-9 (human-typed) to the AI session; `stats` = ai_additions 6, human 0, unknown 0.
   Git AI has no "unknown" bucket; unmarked edits after an AI checkpoint become AI.
2. **With a `mock_known_human` checkpoint** (test 2): lines 8-9 recorded under `h_...` (`humans` section) and stats = ai 4, human 2.
3. Note format (`git notes --ref=ai show HEAD`): file path, indented `s_<session>::t_<trace> <ranges>` / `h_<human> <ranges>`, `---`, JSON metadata.
4. **REAL tool confirmed (Codex CLI, model gpt-6-luna)**: hooks fire and the note names tool `codex`, BUT human lines typed in Notepad after the
   Codex edit were again credited to Codex (stats: ai 6, human 0). In both unmarked cases the over-credited lines sit under a SECOND trace id of the
   same session (`...::t_<A> 4-7`, `...::t_<B> 8-9`). Hypothesis (untested): trace B is the commit-time carry-forward, not a real AI checkpoint.
   Raw checkpoints are archived at `.git\ai\working_logs\old-<base sha>\checkpoints.jsonl` (debug builds never prune) - compare them with the note.
   Editors with the Git AI extension (VS Code/Cursor) should record human saves as known-human; untested.
5. **RAW CHECKPOINTS CONFIRM THE HYPOTHESIS (real Codex run).** `.git\ai\working_logs\old-<base sha>\checkpoints.jsonl` held exactly ONE record:
   kind AiAgent, tool codex, trace `t_f608...`, lines 3-7 (line 3 is a base line Codex touched, probably a line-ending change). Trace `t_31f7...` (lines 8-9, human-typed)
   exists ONLY in the final note, in no checkpoint: Git AI invented that credit at commit time. The raw record is correct; the note is not.
   Consequences: (a) the engine must read raw checkpoints, and treat note claims with unbacked traces as `unknown` (implemented in `prov-gitai::checkpoints` + `classify_added_lines_backed`);
   (b) raw checkpoints are local and short-lived (old-* dirs; release builds prune after 7 days; one dir per base commit), so our LOCAL AGENT must copy them at commit time and store its own
   evidence-based record (e.g. our own notes ref); a CI runner on another machine only sees the pushed Git AI note and cannot repair this by itself;
   (c) the trace-level check cannot catch human lines inside a range whose trace is backed; line-level evidence needs our own editor events/extension.
6. The real raw checkpoint (now a fixture) also contains: `line_stats.additions = 4` (Git AI's own count at the time; the final note claims 6 -> second independent signal of the over-credit),
   `agent_metadata.edit_kind = "bash"` (Codex edited via a shell command), `tool_use_id`, and `transcript_path` (path to the local Codex session file: possible prompt evidence, but privacy-sensitive; do not export).
   Char-level `attributions` mark lines 1-2 human and line 3 onward AI: line 3 was a base line Codex rewrote (probably line endings), so even the raw record can credit rewritten base lines.
Implication: the note alone is not proof. The real engine must also use the raw checkpoints / agent events, and treat
"AI-credited but no pre/post evidence" as lower strength. Caveat: mock preset sends only a post-edit event; real tools also send a pre-edit "human" checkpoint, so real-world over-credit is still UNTESTED. Test with Claude Code / Codex hooks next.
Fixtures for these are in `crates/prov-gitai/tests/fixtures` and pinned by tests.

## What is built in this repo
- `prov-core`: `Category`, `Basis` codes, score bands, `LineCounts`, `PrMetric` (golden test: 30/20/10/5/35 -> 50% / 65%; excluded lines never change %).
- `prov-gitai`: note parser, mapping policy (AI session -> `ai_strong` when verified; unverifiable AI claims -> `ai_probable`; editor markers and conflicts -> `unknown`), exclusion policy.
- `prov-gitai::diff`: parser for `git diff-tree -p --unified=0` -> added line numbers and in-memory added-line text per file.
- `prov-gitai::checkpoints`: raw checkpoint reader + `Backing` (trace ids with real AI evidence), exposing checkpoint file paths, blob hashes, traces, and timestamps.
- `prov-gitai::linecheck`: verifies checkpoint blobs with SHA-256 and aligns blob text against committed file lines after CRLF/LF normalization. Common prefixes and suffixes are trimmed before LCS; middle regions above 4 million DP cells are skipped.
- `prov-gitai` uses the RustCrypto `sha2` crate (MIT/Apache-2.0) to validate blob contents.
- `prov-cli` (binary `prov`): `prov analyze [--repo PATH] [--commit REV] [--json]` reports one commit; `--base REV --head REV` analyzes a PR range using merge-base diff semantics. Merge commits are rejected. JSON outputs use `prov.analysis/0.2`.
- Counting policy `nonblank-v1` excludes empty or whitespace-only additions before placement, blame, and classification; both output formats report `blank_added_lines`.
- PR range reports place surviving added lines on commits using blame; unplaceable lines count as unknown and appear in `unattributed_lines`. `--explain` emits per-line categories and reason codes. Editor marker lines are reported as `editor_marked_lines` with reason code `editor_marker`.
- Evidence loading is shared by single-commit and PR modes. Unreadable notes count as unknown; unverifiable AI claims (missing or unreadable checkpoints, or a claim without a trace id) count as `ai_probable`. For backed AI traces, unchanged blob-aligned lines count as `ai_strong`, replaced lines count as `mixed`, and inserted lines count as `unknown`. Missing or invalid blobs fall back to trace-level classification with a warning. JSON includes the additive `line_check` status, and `--explain` includes `ai_edited_after_checkpoint` and `inserted_after_checkpoint` reason codes.
- End-to-end tests rebuild the real Codex lab scenario and PR range histories in temporary repositories.

## Known limitations
- Single-commit JSON always includes `explanations`; PR mode includes them only with `--explain`.
- Renamed files lose Git AI note matches and are counted as unknown.
- Whitespace-only rewrites by a later commit take over blame credit and are counted as unknown; docs have a `FORMAT_TRANSFORMATION` basis for this later case.
- Blame runs per whole file and can be slow on very large files.
- Raw checkpoints exist only on the developer machine for a limited time.
- The line-level check relies on Git AI's blobs layout: a checkpoint entry's `blob_sha` names `.git/ai/working_logs/old-<parent-sha>/blobs/<blob_sha>`, and that filename must equal the SHA-256 of the blob bytes. Git AI prunes blobs with checkpoints (7 days in release builds).
- Pairing replaced lines within changed regions is a heuristic; repeated or similar lines can yield ambiguous LCS alignments. Large middle regions over the 4 million DP-cell cap fall back to trace-level verification.
- none_recorded relies on Git AI's working_logs layout (an old-<parent> folder with no checkpoints.jsonl); if Git AI changes this or prunes only the file, real AI lines may be counted as unknown (the safe direction). The local agent's own evidence record replaces this heuristic.
- Unverifiable AI claims are `ai_probable` by design (see the ai_probable definition in the docs); they are never `ai_strong`.

Not built yet: broader rewrite lineage, coverage dimensions, evidence levels beyond categories, storage, signing.

## Next steps (order)
0. Run `cargo run -p prov-cli -- analyze --repo C:\dev\lab\real-test --commit HEAD` on the real lab repo (expected: ai_strong 4, unknown 2, 66.7%) and on a commit made with the VS Code extension.
1. Real-tool test: make an edit with Claude Code or Codex in a throwaway repo, commit, save `notes show`/`stats` into `C:\dev\lab\samples`, add as fixtures.
2. Add `git diff base..head --unified=0` parser -> added-line sets; wire to `classify_added_lines`; CLI `prov pr-analyze --base --head --json`.
3. Coverage dimensions + evidence levels (doc 04/05), then own SQLite store (item 4), then connectors/CI/editor items.

## Doc inconsistencies to resolve with the doc owners (found during review)
FR-100 config precedence vs 04 section 17; BRD 50 ms ingest "excluding durable flush" vs 06 "durable ack p95<50ms"; `AnalysisKey` (03 9.2) omits
normalization/score/formula/coverage profiles and correction revision; strict-v1/inclusive-v1 vs the two always-reported metrics;
zero-denominator `0` vs `not_applicable` (code uses not_applicable); score profile v1 lacks rows for patch/rebase lineage, format transforms,
signed corrections, complete-lineage moves; ambiguity "reduce strong to probable" vs "clamped to category"; delivery states (BRD 5 vs FR-093 8);
FR-024 omits sourceInstanceId; runbook 13.3 "mark stale" has no SCM-port method.

## Open design questions
- Where does a known-human line belong? Docs have no human category; adapter maps to `unknown` + separate signal.
- Default category for a Git AI AI-session line: `ai_strong` (current) vs `ai_direct` (needs accepted edit + survival evidence).
- Privacy: confirm whether any data leaves the machine with `api_base_url` default and no login (not verified).
- Known failing Git AI tests on Windows (environmental: timing, symlink privilege, cline hook test) - not investigated.
