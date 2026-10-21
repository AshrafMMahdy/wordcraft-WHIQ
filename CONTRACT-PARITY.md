# Contract parity: WordCraft, GridCraft, DeckCraft (WHIQ forks)

This file is the same in the three forks (branch `whiq`). It records, item by item of the shared
"no dead stops" contract (`craft-fork-contract.md` §1–10), what each app did before the parity
pass of 2026-10-18 and the one shape all three answer with now. The platform's MCP client can
treat the three servers as one: same error codes, field names, flags, `server_status`,
`job_status`, `cancel`, checkpoint and recovered-file names. Domain tools keep their names
(`render_page`, `render_range`, `render_slide`…). Each repository's `WHIQ-CHANGES.md` ("Parity
pass") lists the commits; the contract suites (`tests/mcp-contract/`) check the shared shape
(every error code is checked against the list below on every call).

Legend: **WC** WordCraft, **GC** GridCraft, **DC** DeckCraft. "was → now" where the app changed;
"=" where it already had the shared form.

## 1. The shared surface, item by item

| Contract item | WC | GC | DC | All three now |
|---|---|---|---|---|
| Error object | = `structuredContent.error {code, reason, next_step, …}` | = | = | `isError: true`, `structuredContent.error = {code, reason, next_step, …details}` |
| Error text block | JSON of the error → `[code] reason` / `next_step: …` | `code: reason` / `Next step: …` → shared | = `[code] reason` / `next_step: …` | `[code] reason\nnext_step: …` (+ `\nwarnings: …`) |
| Error codes | `unknown_tool`, `unknown_command`, `bad_params`, `path_refused`, `needs_app`, `needs_password`, `job_not_found`, `parse_error` → mapped; `locked`, `disk_full`, `too_large`, `wrong_password` added | `internal_error` → `internal`, `no_ui` → `unsupported`, `unavailable` → `not_available`, `parse_error` → `invalid_request`, `not_running` (cancel) → a success answer; `out_of_range` added | `forbidden` → `outside_root`; `locked`, `disk_full`, `password_required`, `wrong_password`, `not_available`, `invalid_request` added | 18 codes, nothing else: `busy`, `cancelled`, `disk_full`, `evicted`, `failed`, `internal`, `invalid_argument`, `invalid_request`, `locked`, `no_document`, `not_available`, `not_found`, `out_of_range`, `outside_root`, `password_required`, `too_large`, `unsupported`, `wrong_password` (`contract::code::ALL`) |
| Error details | `closest`, `recovered` → `recovered_path`, `kept_previous` → `previous_files_renamed` | = `closest`, `recovered_path`, `original_path` | `candidates` → `closest`, `recovered`/`reopen` → `recovered_path`/`original_path` | `closest` (near names, ids, files, shapes), `retry_after_ms`, `job_id`, `running`, `recovered_path`, `original_path`, `read_root`/`write_root` (on `outside_root`), `results`/`failed_at`/`stopped_at`/`rolled_back` (batches) |
| Unknown tool | tool result `unknown_tool` → `not_found` | JSON-RPC `-32602` → tool result `not_found` | = tool result `not_found` | a tool result with `not_found` and `closest` |
| Protocol errors | = `error.data` holds the error object (2026-10-20: also the worker's own: unknown method, `tools/call` without a name, `resources/read`) | = | plain message → `data` added (2026-10-20: also a line that is not JSON) | JSON-RPC errors carry `{code, reason, next_step}` in `error.data` |
| `busy` | answered at once → waits `--busy-after-ms` in order, then `busy` | = waits 300 ms (`busy_grace`), flag added | queued 8000 ms → 300 ms | a call arriving while another runs waits at most `--busy-after-ms` (default 300, for pipelined requests), then answers `busy` with `retry_after_ms`, `running` and the `job_id` when the running call is a job |
| Long call → job | `--job-after-ms` 15000, `background: true` | `--call-timeout-ms` 20000 → `--job-after-ms` 15000 | none → `--job-after-ms` 15000 added | a call still running after `--job-after-ms` (15000) answers `{job_id, status: "running", tool, retry_after_ms, next_step}` and carries on; `async: true` on any tool makes it a job at once (WC still reads `background`) |
| Big open → job | none → `--open-job-mb` (20) added | `--job-open-mb` → `--open-job-mb` (20) | = `--open-job-mb` 24 → 20, decimals | opening a file of `--open-job-mb` (20) MB or more is a job from the start; domain thresholds stay: GC `--job-cells`, DC `--export-job-slides` |
| Client gives up (`notifications/cancelled`) | = the call becomes a job | stopped the call → becomes a job | ignored → becomes a job | the call carries on as a job, its answer kept for `job_status` (clients send this on their own timeouts); `cancel` is what stops work |
| `job_status` | tool's result + `job` → shared | `{status, result\|error, is_error}` → shared | `state`, `kind` → `status`, `tool` | `{job_id, tool, status: running\|done\|failed\|cancelled, elapsed_ms, progress, stage, steps, stages, retry_after_ms (running), next_step, result (done) \| error (failed, cancelled)}`; a finished job's pictures come back as content blocks; unknown id → `not_found` + `closest`; ids `job-N`, also `N`, `JOB_N` |
| `list_jobs` | none → added | `job_status {}` only → tool added | = (array → `{jobs}`) | `list_jobs` and `job_status` without an id answer `{jobs: [summary…]}` |
| Progress | `{phase, done, total, elapsed_ms}` → shared | none → `gridcraft_core::progress` (recalculation, cell writes, sheets read/written, PDF pages) | = | `progress` 0..1 (never goes back, 1 only when done), `stage`, `steps {done, total}`, `stages` (passed so far, current last) on every job and in `server_status.current_call` |
| Progress notifications | = `notifications/progress` for a `progressToken` | none → ported | none → ported | a request with `_meta.progressToken` gets `notifications/progress`: the value only grows (steps done, or a beat each second), `total` when counted, a message with the stage and seconds |
| `cancel` | `{cancelling}` / `{cancelled:false}` / `job_not_found` → shared | `{status: cancelling}` / errors `not_running` → shared | `{cancelled, running}` / `{job_id, state}` → shared | `cancel {job_id?, queued?}` answers `{cancelled: bool, status: cancelling\|not_running\|done\|failed\|cancelled, job_id, tool, next_step, retry_after_ms?, dropped_from_queue?}`; unknown job → `not_found`; the stopped call answers `cancelled` (job status `cancelled`, `error.code` `cancelled`) |
| Rollback on cancel | single commands; batch and multi-paragraph `type_text` kept the steps done → whole call undone (`rolled_back`) | = the workbook as before the call | single commands; a batch kept its first commands → whole batch undone | a cancelled call leaves the document as it was before the call; read-only multi-step calls (WC `render_page` over a range) answer `continue_from` instead |
| Checkpoint triggers | = `--checkpoint-edits` 20 / `--checkpoint-secs` 60 | = 20 / 60 | 10 / 30 → 20 / 60 | `--checkpoint-edits N` (20), `--checkpoint-secs S` (60), 0 turns either off |
| Checkpoint file | = the recovered file itself | = | `<name>.recovered-checkpoint-<id>.<ext>` (separate) → the recovered file itself | `<name>.recovered-<stamp>.<ext>` in the write root, rewritten atomically in place; a later auto-save of the same stretch of work updates that file; removed once the document is saved (unless it was reported) |
| Recovered file name | `…recovered-20261017T193012Z.docx` → shared | `…recovered-20261017-193012-123.xlsx` → shared | = `…recovered-20261017-193012.pptx` | `<name>.recovered-<YYYYMMDD-HHMMSS>[-N].<ext>` (UTC; `-2`, `-3` if taken) in the write root, else the document's folder |
| Recovered files reported | marker → `evicted` on the next process's first call → reported, not failed | notice in `warnings` → `recovered_files` | = `recovered_files` | `recovered_files: [{path, reason, original, at}]` on the next tool answer (each named once, plus a text line), in `server_status`, and in `initialize` instructions; `reason`: `checkpoint`, `signal`, `disconnect`, `panic`, `memory`, `idle`, `evict`, `close`, `found` (left by an earlier process; the write root is scanned at start) |
| Eviction | memory only; `evicted` once → idle (`--idle-evict-secs`) and `evict` tool added; `evicted` until a document is opened | `--evict-idle-secs` → `--idle-evict-secs`; `evict` tool added | = idle, memory, `evict` | `--idle-evict-secs`, `--max-memory-mb` (OS-measured resident memory) and the `evict` tool auto-save and close; every call that needs the document answers `evicted` with `recovered_path` / `original_path` until one is opened or started; `server_status.evicted: [{name, reason, recovered_path, original_path}]` |
| Memory and file size (2026-10-21) | layouts and decoded pictures shed first; `inspect_document` / `render_page` lay out a page window (a few pages in memory); `.docx` read part by part from the file; `export_text` | sheets stream from the zip and into it; ~105 bytes a cell (was ~235); a read stops at the budget with `too_large`; the answer of the call after which memory was over says `closed_after_this_call`; `export_range` | media from 8 MiB (video, sound) and 64 MiB (pictures) kept in spill files on disk and streamed into every save; package read part by part from the file; parts not kept reported (`damagedMedia`, `unreferencedParts`); no eviction while a call or job runs | No tool is limited by file size: files are read and written as streams; every in-memory limit is a share of the `--max-memory-mb` budget (floors: the old fixed values); a limit reached is reported with its numbers, never a silent cut. Memory eviction happens only when no call runs (never during a write). A `save_document` / `save_workbook` / `save_presentation` (or `file.save`) of a document the server closed writes its auto-saved copy (or unchanged original) where asked, byte for byte (`saved_from`). `roundtrip_check` compares binary parts by zip CRC |
| `server_status` | `document`, `busy`, `recovered` → shared | `document`, `busy`, `checkpoint` → shared | `rss_mb`, `limit_mb`, `checkpoints.files` → shared | `{server, version, protocol, pid, uptime_ms, backend, author, memory, fonts, roots {read, write}, documents [{name, path, dirty, active, …domain counts}], current_call, jobs, recovered_files, evicted, checkpoints {every_edits, every_secs, pending_edits, writes, files [{path, document, …}]}, idle_ms, calls, busy_replies, limits {max_result_bytes, max_memory_mb, busy_after_ms, job_after_ms, open_job_bytes, idle_evict_secs, …domain}}` |
| `memory` | `{rss_bytes, rss_source, document_bytes, limit_bytes}` → shared | `{rss_bytes}` → shared | `{rss_bytes, rss_mb, committed_bytes, limit_mb, measured, source}` → shared | `{rss_bytes, committed_bytes, limit_bytes, measured, source}` from the OS (`memory-stats`: Windows `GetProcessMemoryInfo`, macOS `task_info`, Linux `/proc/self`), the same `source` words in every app |
| `fonts` | none → added | `{faces, families, default}` → shared | none → added | `{source: "craft-fonts", faces, families, default, arabic}` |
| Trimmed results | = `truncated`, `continue_from`, `total`, `next_step`; resume with `continue_from` | resume with `offset` → `continue_from` too | resume with `offset` → `continue_from` too | over `--max-result-bytes` (or the call's `max_bytes`) the largest list is cut: `truncated: true`, `continue_from`, `total`, `next_step`; pass `continue_from` back to any tool (`offset` still works on list tools) |
| Common arguments | `caller`, `background`, `overwrite`, `continue_from`, `max_bytes` → `async` added | `caller`, `async`, `overwrite` → `continue_from`, `max_bytes` added | `caller` (+`async` on two tools) → all added | every tool takes `caller`, `async`, `continue_from`, `max_bytes`; tools that write files take `overwrite` |
| Forgiving inputs | names in any spelling, aliases, coercion → argument synonyms (`file`→`path`…), enum values in any case, lists given as text | = (largest synonym table) | names, aliases, coercion → enum values in any case, lists given as text; non-object arguments refused | tool names, command ids and argument names in any case or spelling (`renderSlide`, `render-slide`), short aliases (`open`, `save`, `status`, `jobs`, `stop`), synonyms (`file`→`path`, `job`→`job_id`, `offset`↔`continue_from`), numbers and booleans given as strings, enum values in any case, a list given as `"a, b"`; unknown arguments → `warnings` on a successful call; arguments that are not an object → `invalid_argument` |
| OLD_ rename | `OLD_<name>`, `OLD_2_<name>`; `kept_previous` → shared | `OLD_<stem>-2.<ext>`, own earlier saves replaced silently, `saved[].previous_renamed_to` → shared | = `OLD_<name>`, `OLD_2_<name>`, `previous_files_renamed`, `notice` (+ warning added) | a save or export over an existing file renames it `OLD_<name>` (then `OLD_2_<name>`, `OLD_3_<name>`…, the server's own earlier saves too) unless `overwrite: true`; the answer has `previous_files_renamed: [{path, previous}]`, a `notice` and a warning; saves are temp + fsync + rename |
| `--author` / `caller` | changes, comments, last modifier → creator when empty added | = notes, comments, `lastModifiedBy`, `creator` when empty | = comments, `lastModifiedBy`, `creator` when empty | `--author NAME` at start, `caller` per call: comments / notes / tracked changes carry it; a save stamps the last modifier and, when the document has none, its creator |
| Roots | = | = | = | `--automation-read-root`, `--automation-write-root`, links followed, symlink escapes refused (`outside_root`); relative paths: WC and DC inside the write root (the server's working folder), GC inside the root of their kind (the same folder when the platform sets both to the chat's folder) |
| OLD_ off for a supervisor (2026-10-20) | none → `--automation-no-old-copies` | not yet (GC is a separate change) | none → `--automation-no-old-copies` | with the flag, saves and exports replace files: no `OLD_` copies, no `previous_files_renamed` (the platform keeps the backups); `server_status.limits.old_copies` says which (true by default) |
| File arguments (2026-10-20) | none → `_meta["whiq/files"]` | not yet | none → `_meta["whiq/files"]` | a tool that reads files names them in `tools/list`: `_meta: {"whiq/files": {"read": [argument names], "opens": bool}}` — `read` are the arguments naming existing files the call reads (to stage inside the read root), `opens` that the call opens or starts the document (the file of the first `read` argument, when given); tools that read no file have none. WC: `open_document`, `import_markdown` (`path`, opens), `new_document` (opens), `roundtrip_check` (`path`), `compare_documents` (`revised`, `original`); DC: `open_presentation` (`path`, opens), `new_presentation` (opens), `build_deck` (`template`, `logo`, opens; its `path` is the output), `adopt_template`, `roundtrip_check`, `insert_picture` (`path`) |
| Document handles (2026-10-20) | one document per process → `doc-<n>` handles: open / new / import answer `document`, the open one stays open beside it | `doc-<uid>` (open / new / build_workbook) | window index → `doc-<uid>` (open / new / build_deck; the index stays as `index`) | every document tool takes `document` (the handle, or its number); the active one comes back after the call; `server_status.documents[].document`; `close_document` / `close_workbook` / `close_presentation {document?, discard?}` refuse unsaved changes unless `discard: true`; an unknown handle is `not_found` with `open_documents`; every dirty document is recovered on SIGTERM, hang-up, panic and eviction |
| Annotations | `openWorldHint` = | = | = | `readOnlyHint` only on reads, `destructiveHint` on what can delete or overwrite, `idempotentHint` where true, `openWorldHint: false`; shared tools: `server_status`, `job_status`, `list_jobs`, `roundtrip_check` read-only; `cancel`, `evict` not read-only, not destructive, idempotent (GC `cancel` was destructive) |
| `roundtrip_check` | = (docx story diff) | none → OPC diff added | none → OPC diff added | open + save in memory, report `{lossless, missingParts, addedParts, changedBinaryParts, missingRelationships, renamed_parts, stories [{part, identical, losses…}], problems}`; GC and DC compare parts, relationships, content types, binary parts and each XML part's element inventory |
| Bidi default | = | = | = | a paragraph or cell with no direction takes its base direction from its first strong character (Arabic: RTL, right-aligned) |
| Fonts | `CRAFT_FONTS_DIR`, `CRAFT_FONTS_REQUIRED`; pin 8dcdacd → 43913056; `.cargo/config.toml` default added | pin abb8331 (release.yml) / 43913056 (recipe) → 43913056 | = 43913056 | craft-fonts **43913056ce82a3fdad0ef69f6b88d1dc922a8104** in every recipe and `whiq-release.yml`; `CRAFT_FONTS_DIR` (local default `../craft-fonts`), `CRAFT_FONTS_REQUIRED=1` for releases |
| Release | upstream `release.yml` (signed, secrets) | = | = | new `.github/workflows/whiq-release.yml`: tags `whiq-v*` or dispatch; `<app>-cli` only, Linux x86_64 musl (static) and Windows x86_64 (static CRT); contract tests and smoke against the built binary; release assets `<app>-cli-<tag>-linux-x86_64.tar.gz`, `<app>-cli-<tag>-windows-x86_64.zip`, `SHA256SUMS`, `engines.lock` (format: `engines.lock.example`) |
| Test binary override | `WORDCRAFT_CLI` | `GRIDCRAFT_CLI` | none → `DECKCRAFT_CLI` | `<APP>_CLI=path` runs the contract suite against another binary (the release workflow's) |

## 2. The flags of `<app>-cli mcp` (the same names in the three)

| Flag | Default | WC before | GC before | DC before |
|---|---|---|---|---|
| `--author NAME` | none | = | = | = |
| `--automation-read-root DIR`, `--automation-write-root DIR` | none | = | = | = |
| `--busy-after-ms N` | 300 | — (0) | — (300, fixed) | = 8000 |
| `--job-after-ms N` | 15000 | = | `--call-timeout-ms` 20000 | — |
| `--open-job-mb N` (decimals) | 20 | — | `--job-open-mb` 20 | = 24 |
| `--idle-evict-secs N` | off | — | `--evict-idle-secs` | = |
| `--max-memory-mb N` | off | = | = | = (floored at 16) |
| `--max-result-bytes N` | WC 100000, GC/DC 262144 | = | `--max-result-kb` | = |
| `--checkpoint-edits N` / `--checkpoint-secs N` | 20 / 60 | = | = | = 10 / 30 |
| `--automation-no-old-copies` | off (OLD_ copies kept) | — (added 2026-10-20) | — (not yet) | — (added 2026-10-20) |
| domain | | | `--job-cells`, `--max-read-cells` | `--export-job-slides`, `--sample` |

The older GC names (`--call-timeout-ms`, `--evict-idle-secs`, `--job-open-mb`, `--max-result-kb`)
are still read.

## 3. Still different, on purpose

- Domain tools and domain arguments (`render_page` / `render_range` / `render_slide`, …), and
  domain job thresholds (`--job-cells`, `--export-job-slides`).
- GC `read_range` continues with a cell address: `continue_from` is the next cell and
  `next_range` the range to ask for (a 2-D range is not a list).
- All three hold several documents per process (`documents` lists them, one `active`), named by
  `document` handles (2026-10-20; WC held one before). DC opens and exports on job threads, so its document tools answer `busy` (with the
  `job_id`) while a presentation is still opening.
- `--max-result-bytes` defaults: WC 100000, GC and DC 262144 (WC's answers carry rendered
  pages separately, capped at 16 MB of PNG).
- How the waiting call waits: WC and DC keep it in a queue, GC's reader waits for the running
  call; all three answer `busy` after `--busy-after-ms`.
- Test hooks (`<APP>_MCP_TEST_HOOKS=1`): WC `__sleep {ms}` / `__panic`, GC `debug_sleep {ms}` /
  `debug_panic`, DC `_delay_ms` / `_panic` arguments. Test-only, never in normal use.
- Which fonts a release build requires (`CRAFT_FONTS_REQUIRED=1`): WC Arabic, Japanese and
  Simplified Chinese (it reads `fonts/manifest.txt` only); GC Arabic and a Latin family with four
  styles; DC Arabic, Japanese, Latin and real italics (both read `fonts/latin-manifest.txt` too).
  The one pin, 43913056, satisfies all three: its `fonts/manifest.txt` is byte for byte the one of
  8dcdacd, which WordCraft pinned before.
- Not observed: the Windows and macOS memory readings and the Windows build were type-checked
  and are built by `whiq-release.yml`, but this pass ran everything on Linux x86_64 only.
