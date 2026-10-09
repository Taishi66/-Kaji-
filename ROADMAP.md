# Kaji roadmap

Updated 2026-10-09. This is the repository view of the maintainer's canonical
project roadmap. Historical delivery records and architectural decisions remain
in the maintainer's vault.

**Resumed at the maintainer's request, 2026-10-09.** Changes are preserved.
The preceding lot was published as `3b9e3189a` on the tracked
`feat/kaji-init` branch. The published source excludes earlier forge,
mission-control and Git-display edits and generated graph files, which stay local.
Its isolated CLI library suite passes **1,146 tests, zero failures, two ignored**.
Strict lean CLI clippy on all targets also passes for that isolated source.
The document preview/suggestion lot passed the lean CLI library suite, strict
clippy and a rebuilt debug terminal. The earlier minimal-terminal baseline also
passed default-feature tests and produced optimised executables and matched
runtime measurements. Those executables and measurements precede this lot.
Remaining workspace-wide checks are required before release/merge.

## Current priority: the terminal

A minimal, polished terminal that makes tasks, activity and decisions easy to
understand. The maintainer clarified that runtime speed, responsiveness and RAM
efficiency take priority over executable size. Disk footprint is secondary if
the runtime benefits are measured.
The desktop follows terminal work. Existing UI copy stays in English; kanji remain
visual accents accompanied by meaningful text.

## Implemented and locally validated

| Change | State | Evidence |
| --- | --- | --- |
| Short welcome; complete help on demand | Validated | `tui/mod.rs`, startup render tests |
| Shared, short command descriptions | Validated | `COMMANDS` in `tui/app.rs` |
| One height measurement per chat line | Validated | `tui/ui.rs`, loader regression test |
| Separate lean build | Configuration written | `profile.lean`, `just release-lean`; features documented in README |
| Persistent permission-mode name | Validated | `tui/statusbar.rs`, mode and highlight tests |
| Standard text by default; optional Nerd Font icons | Validated | `tui/icons.rs`, `App::new`, resolver tests |
| Labelled tokens, agents and thinking; activity preserved as the bar narrows | Validated | `tui/statusbar.rs`, `tui/ui.rs`, narrow-bar test |
| Compact, responsive command palette | Validated | Six visible entries, stacked descriptions in narrow columns, selection/render tests |
| Bounded Markdown render cache | Validated | At most 256 recent answers and 1 MiB of retained payload; source maps included, exact text/width/theme invalidation; explicit RAM/CPU tradeoff |
| File completion sends on the next Enter | Validated | File dropdown closes; directory completion keeps its children |
| Readable tool grant before approval | Validated | Command and refusal visible at 80×24 and 40×24; anti-masking tests retained |
| Honest headless failure result | Validated | Typed provider failures exit 1 in text/JSON/stream-JSON, both engines; success still exits 0 |
| Hide internal context on resume/restore | Validated | Uses the typed metadata; user text and stored history are preserved |
| Kaji-specific setup documentation | Written | README |
| Local document previews through `/open <path>` | Validated | Code/UTF-16, DOCX, ODT, PPTX, XLSX, PDF text and PNG/JPEG/WebP; bounded worker and hostile-input tests |
| Readable documents across terminal widths | Validated | Cached Unicode wrapping, last-page resize regression, read-only document footer |
| Suggestions off by default, `/suggest on\|off` | Validated | Current chat context, bounded streamed output, cancellation and failure cleanup; localhost terminal checks |

Validation and subsequent publication were explicitly authorised on 2026-10-09.
The preceding preview/suggestion working tree passed **1,157 tests, zero failures, two ignored**.
Strict lean CLI clippy and debug compilation pass. The previous baseline passed
1,130 tests in both lean and default-feature configurations; default-feature and
workspace checks have not been repeated for the latest lot.
That 1,157 count includes 11 tests from the earlier local edits excluded from
the published commit. Its isolated source passes 1,146 tests instead;
the test count difference is deliberate, not skipped tests.
One ignored test is the explicit render timing comparison, run separately.
Earlier local changes to the
forge thread, mission control and Git display are preserved and are not claimed
as delivered by this session.

## Earlier minimal-terminal acceptance steps

Completed checks below belong to the baseline before the document/suggestion
lot. Outstanding release, recipe and comparative checks remain applicable.

- [x] Build the lean-feature debug executable and launch its terminal.
- [x] Complete initial optimised lean and full builds; final fixes require rebuilt artifacts.
- [x] Run the lean CLI library tests, including welcome, palette, modes, Unicode,
  loader, cache invalidation and turn navigation (1,130 passed).
- [x] Run clippy on all lean CLI targets with `-D warnings`, including headless failure handling and resume/restore metadata filtering.
- [x] Run the default-feature CLI library suite in release mode (1,130 passed, zero failed, two ignored).
- [ ] Check the remaining workspace before release or merge.
- [x] Run the manual debug render comparison: 32 answers, 9.340 → 3.975 ms/frame;
  128 answers, 28.074 → 23.045 ms/frame (80×24, 200 frames per variant).
- [x] Inspect real startup and command palette at 80×24 and 40×24 in an isolated
  tmux terminal (before the final status-bar spacing change).
- [x] Inspect wide/light layouts, readable approvals, denial and mode changes on the rebuilt debug binary.
- [x] Finish the optimised lean binary containing all final fixes; eight real-binary headless checks pass.
- [x] Finish the full optimised binary containing all final fixes.
- [x] Measure the final release renderer: 32 answers 0.711 → 0.277 ms/frame;
  128 answers 2.734 → 2.725 ms/frame (no material long-history gain established).
- [x] Verify real startup, session resume, file attachment and interruption using an isolated localhost provider.
- [x] Verify the final metadata filter hides internal context on session resume with the rebuilt binary.
- [x] Attempt the updated self-test recipe; the configured model returns HTTP 410 (retired).
- [ ] Obtain a successful recipe run with an available model; no provider configuration was changed.
- [ ] Extend the baseline to peak RSS, much longer sessions, input latency and
  tool workloads in both engines. Compare competitor runtime, task success and
  token cost using the same model before claiming competitive superiority.
- [x] Establish a matched 32-turn runtime baseline: lean/full startup medians
  86.83/86.02 ms (warm), idle RSS 44.47/49.00 MiB, after 32 answers 105.20/109.84 MiB,
  local turn latency 141.65/141.66 ms. Corrected fixture handles ancillary calls;
  counts verified: 192 main turns, 192 suggestions and six memory curations.
  Small RSS saving, no meaningful runtime speed advantage established.

The matched runtime fixture uses the same 32 Markdown answers, fresh session and
memory stores, 80×24 terminals and three alternating passes per variant. It measures
startup to a usable terminal, process-tree RSS and local turn-to-display
latency. This isolates local harness overhead; it is not a model-quality or
cold-machine benchmark. Runtime performance and RAM determine the preferred
distribution; binary size alone does not.

The maintainer explicitly requested builds, tests and clippy on 2026-10-09.
The local `sccache` wrapper fails even outside the sandbox; validation uses direct
compilation with `RUSTC_WRAPPER=`. Validation resumed at the maintainer's request.
Rust 1.96.1 on macOS 27 rejected the stripped `sqlx-macros` dylib. An explicit
`strip = "none"` override for this dev dependency fixes the test build; lean
build-time dependencies have the same exemption, with final binary stripping kept.
The existing `target/debug/deps` contains 893,903 entries (directory enumeration
took 38.85 seconds). Lean validation uses `/tmp/kaji-validation-2026-10-09`;
the full release build reuses the existing `target/release` cache. Lean builds use four jobs; the full build uses two jobs, with incremental compilation disabled. Existing build
artifacts are preserved; a disk-retention policy needs separate review.
Both debug and release render workloads and a matched 32-turn runtime baseline
are measured. Competitor runtime and task evaluations remain pending. Real
terminal checks passed on the final debug binary. See the
[validation report](docs/superpowers/reports/2026-10-09-terminal-validation.md).

Resume commands and environment details are in that report. The previous baseline passed
1,130 CLI library tests, lean CLI clippy and five targeted security integration tests. No commit, push or installed-binary
replacement was performed. Preserve the earlier unrelated local edits when resuming.

## Code, chat and documents — latest validated lot

The maintainer chose previews inside the terminal, according to its graphical
capabilities. `/open <path>` loads locally without a model request. One dedicated
reader and a latest-request queue keep loading out of the event loop; stale
results cannot reopen a closed viewer or replace a newer file. Reload failures
preserve the previous readable snapshot. Rich documents are read-only previews;
Unicode wrapping is cached for the current width. PNG/JPEG/WebP use colour half
blocks with a retained thumbnail of at most 160×160 pixels.

Office previews extract text without running macros, formulas or external links.
ZIP entry count, central-directory size and XML expansion are bounded; DTDs are
rejected. PDF text conversion requires `pdftotext`, uses a private bounded input
snapshot, clears inherited secrets, times out and reaps its child. Converter
resource limits vary by platform; there is no hard converter RAM limit on macOS
and this is not an OS sandbox. Graphics, layout, OCR and spreadsheet formatting
are not reproduced. Explicit Office/PDF model attachments are implemented in
the subsequent lot below.

- [x] Suggestions disabled by default and explicitly selectable; current recent
  chat replaces the stale initial context. At most one owned task, cancelled on
  editing/new turns/exit; 10-second timeout and bounded context/output.
- [x] Nine local terminal previews make zero provider requests. Five fixture
  chat turns validate suggestion acceptance, current context, editing cancellation,
  error cleanup and disabling; 40×24 and 120×30 previews and last-page reflow pass.
- [x] Lean CLI tests: **1,157 passed, zero failed, two ignored**; all-target clippy
  `-D warnings` and debug build pass with the locked dependency graph.
- [x] Updated self-test recipe attempted: configured retired model still returns
  HTTP 410 and exit 1. The recipe has not passed.
- [ ] Account for ancillary inference, including provider retries, in the usage
  ledger. The failure fixture produces four HTTP attempts for one suggestion.
- [ ] Repeat default-feature/workspace verification before release; build the
  optimised artifacts containing this lot. No new runtime/RAM gain is claimed.

Evidence and limitations:
[document validation report](docs/superpowers/reports/2026-10-09-documents-validation.md).

## Security and robustness — highest priority

The maintainer explicitly requested maximum security robustness and an honest
comparison with other harnesses. Permissions alone are not OS isolation. The
current native shell path executes a host shell. Declared provider credential
environment inheritance is filtered in the new shared shell path;
`configure_subprocess` manages process groups, not filesystem/network confinement.

- [ ] Define and enforce a sandbox boundary for shell commands and their child
  processes; test blocked writes, reads and network egress, symlinks and escape
  attempts. Keep an explicit, reviewable path for authorised exceptions.
- [ ] Keep provider credentials outside untrusted subprocesses; audit logs,
  exports, memory recall and MCP boundaries using fake-secret fixtures.
  Limited shell inheritance filtering is implemented: current builtin/custom
  provider secret-key names plus BASH_ENV/ENV are removed from native children,
  Flatpak host/launcher and login-PATH probes; custom refresh is transactional.
  This leaves profiles, files/keychain, undeclared keys and MCP/hooks outside its
  boundary. The broader milestone remains open. Evidence:
  [shell environment validation](docs/superpowers/reports/2026-10-09-shell-provider-environment-validation.md).
- [ ] Verify denied requests stay denied in both engines, including delegation,
  retries and chained/substituted shell commands. Five existing security tests
  pass; this does not constitute a complete audit.
- [ ] Test cancellation/timeout of child processes and resource limits against
  hostile or oversized tool/model output, beyond the bounded display cache.
- [ ] Audit dependency advisories and update verification for both build variants;
  document supported platforms and failure behavior. No complete audit ran here.
- [x] Prevent typed provider/stream errors from being reported as headless success;
  real binary checks pass for both engines and all three output formats.
- [ ] Provide a clear recovery path for unavailable/retired models, and measure
  cold startup plus idle/long-session RAM and disk growth with controlled fixtures.

The target is evidenced security properties, never a promise of zero vulnerabilities.

## Following work

**Current lot:** explicit Office/PDF model attachments are implemented outside
the terminal input loop, with a single worker, one queued request, a six-second
deadline, quoted filenames and visible refusal/truncation. Text retains existing
64 KiB/file and 256 KiB total limits, with at most 32 references. Cancelled
preparation cannot publish stale notices or start a model request. Ordinary chat,
queued steering and the first goal work prompt share the preparation path.
Exact staged-source validation: **1,157 passed, zero failed, two ignored**;
strict all-target CLI clippy and formatting checks pass. Working-tree validation:
**1,168 passed, zero failed, two ignored**, including 11 earlier unrelated tests
excluded from publication; strict CLI clippy and debug build pass. Feature recipe
attempt remains HTTP 410 / exit 1.
Actual terminal: **20 primary messages in the two engines**, two curations;
extracted payloads, truncation/refusal, quoted reader gesture, responsive typing,
Esc and whole-preparation timeout verified. Cancelled/timed-out preparations send
zero model requests and reap their converter. Source edits do not alter past
attachment snapshots. No new optimised runtime or RAM claim.
Design: [document attachments](docs/superpowers/specs/2026-10-09-document-attachments-design.md).
Evidence: [attachment validation](docs/superpowers/reports/2026-10-09-attachments-validation.md).

Audit ancillary inference token-cost accounting and retries. Optional suggestions,
current context, bounded output and cancellation are implemented and validated.
Asynchronous Office/PDF attachments with explicit budgets and visible truncation
are implemented. Prioritise shell/credential isolation, ancillary usage accounting
and long-session/attachment-history profiling before a competitive claim.
Evaluate native terminal image protocols and OCR
only with measured costs, safe cancellation and readable fallbacks. Keep preview
separate from model attachment, and document format/layout limitations.
Profile shared initialisation and history allocations. Assess rendering
much longer histories: the release cache gain at 128 answers is negligible.
Improve activity and error summaries before exposing advanced
details. Finish the state-machine migration with parity checks and replay coverage,
then remove the legacy loop. Carry forward the earlier roadmap's robustness work:
turn-start exclusivity, workflow shutdown on errors, deterministic tests, remaining
feature-gated failures and live control of workflow agents. Reassess each item
against the current code before marking it complete.

Desktop Tauri v2 on ACP remains planned, after the terminal priorities.

Scope and verification design:
[2026-10-09 terminal design](docs/superpowers/specs/2026-10-09-terminal-minimal-design.md).

## Responsive terminal reading — validated implementation

The prior functional checks remain evidence of their own scope, while the user
rejected the previous visual readability. Visual acceptance is reopened. Clean
40/80/120/200-column renders received Astra/root review; user acceptance remains
pending. Tables preserve full cell content, use available width with prose-cell
measures ≤96 and label measures ≤28, and become readable records at 40 columns.
Prose wraps at 88; code uses the available width. The compact single-line composer
keeps horizontal caret scrolling. Logical source anchors preserve reading through
resize, and scroll gestures use the effective resized position.

A bounded 256-entry / 1 MiB render cache replaces 32 / 128 KiB, adding 896 KiB to
the retained payload bound to avoid repeated history misses. Controlled debug
128-answer warm frames measure 14.432/23.717 ms at 40/200 columns with zero cache
misses. This is a RAM/CPU tradeoff, not an RSS, optimized-runtime or superiority
claim. Working-tree TUI868/3 ignored, strict CLI clippy and debug build pass;
state-machine terminal resize/follow-bottom checks pass. Exact published-source CLI tests pass **1,155/0 failed/4 ignored** and strict
all-target clippy passes. The recipe provider responded, but the run was
intentionally interrupted after a sccache/environment error; it has not passed.
[Responsive reading evidence](docs/superpowers/reports/2026-10-09-responsive-reading-validation.md).
