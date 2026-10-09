# Document attachment validation — 2026-10-09

This lot follows the published terminal/preview/suggestion commit `3b9e3189a`.
The maintainer requested continued roadmap work. Earlier unrelated forge,
mission-control, Git-display and generated graph edits remain preserved.

## Current result

The exact staged-source snapshot passes **1,157 lean CLI library tests, zero
failures, two ignored** in 4.33 seconds, and strict all-target CLI clippy passes.
Its four touched Rust files pass the formatting check. The working-tree suite
passes **1,168 tests, zero failures, two ignored** in 4.33 seconds; strict
all-target CLI clippy and debug build pass there too. The working-tree count
includes 11 tests from earlier unrelated local edits excluded from publication. Default-feature
and full workspace checks have not been repeated. No optimised runtime benchmark,
complete dependency/security audit or superiority claim belongs to this lot.

The rebuilt debug executable SHA-256 is
`9efec62acd30f031e2be08094dc6249c6de3527ae8bd504b087634c58d50a7fe`.
Hermit/Rust 1.96.1, macOS 27, Apple M2; direct compiler, incremental disabled,
four jobs, target `/tmp/kaji-validation-2026-10-09`, locked offline dependencies.
No dependency changes were needed. Touched Rust files were formatted individually.

## Behavior

Explicit DOCX/ODT/PPTX/XLSX/PDF mentions now attach extracted text rather than a
binary-skipped placeholder. `/open` still previews locally without sending.
Text attachments share **64 KiB per file / 256 KiB combined**, including their
envelopes, with at most 32 references per submission. Source and extraction
budgets are the previously validated document reader budgets. Oversized or
malformed documents show refusal/truncation notices. Images retain their existing
separate caps. Filesystem symlinks to ordinary files remain supported.

Quoted mentions and reader/finder/paste attachment gestures support filenames
with spaces. Quoting is parsed as data without shell execution or expansion;
attachment-path attributes escape XML-sensitive characters. An unterminated
quoted mention stops parsing additional references inside that quoted text.

Normal submission, queued steering and the first goal work prompt share one
attachment worker and one queued slot. Mention-free chat completes without file
I/O. Preparation has a six-second deadline; source reads and parsing happen off
the terminal event loop. The pending state distinguishes attachment preparation
from agent startup. Only a still-current successful preparation constructs the
model message and publishes notices/image placeholders. Dropping preparation
signals cancellation; PDF processes observe it and are killed/reaped. A cancelled
or timed-out preparation does not invoke `Agent::reply`.

A stalled filesystem read can outlive cancellation on that one worker; repeated
submissions cannot create unbounded worker threads. A full queue fails clearly
instead of waiting in the input loop. Owner shutdown signals an active preparation
and waits at most 100 ms. The worker is separate from Tokio's blocking pool.

Both engines receive the same resolved ordinary user message, whose attachment
text is persisted by the existing message path. A later source edit does not
retroactively alter the user-message snapshot. No core loop, implicit external
prompt-state source or replay event kind was added.

## Verification

Unit coverage includes Office formats and sparse cell addresses, retained byte
budgets and Unicode truncation, hostile ZIP/XML refusal, reference counts,
literal shell characters and quoted paths, cancellation of running/queued work,
bounded queue failure, owner shutdown, dropped polled preparation, and shared
ordinary/goal/queued submission behavior. Earlier converter timeout, environment
removal and process reaping tests remain in the suite.

Actual terminal checks use isolated synthetic documents, an OpenAI-compatible
localhost fixture and both `KAJI_STATE_MACHINE=0` and `1`. Metadata checks inspect
the actual provider-bound message, not just the visible transcript. Real Poppler
extracts the valid PDF; a local sleeping converter fixture exercises cancellation
and deadlines without an external model or document renderer.

The final fixture delivers **20 primary messages, ten per engine**, plus two
memory-curation requests. Office/PDF/code payloads, sparse XLSX cells, a 64 KiB
truncation, malformed archive refusal and the quoted reader gesture are checked
at the provider boundary. Typing stays responsive during a sleeping converter.
Both Esc and the six-second whole-preparation deadline produce **zero model
requests** and reap their converter; a subsequent plain message succeeds. A
source edit is absent from the already persisted attachment history. Preliminary
fixture runs were discarded after correcting completion key handling and assertions
for wrapped terminal lines; their request counts are not added to the final result.

The updated self-test recipe was attempted with the rebuilt binary. The configured
`deepseek-v4-flash:0731` is still retired: **HTTP 410, exit 1**. The recipe has not
passed; no provider or credential configuration was changed to bypass the failure.

PDF output covers up to 20 pages without OCR. Office previews/attachments do not
reproduce styles, page layout, date/number formats or relationship ordering, and
do not execute macros, formulas or external links. Existing converter/image
limits have platform-dependent and best-effort aspects; this work is not an OS
sandbox. Windows/Linux execution, arbitrary document compatibility, longer-session
RAM/input latency and competitor task/cost comparisons remain unverified.

Fixture metadata and final terminal results are recorded in
[attachment results](2026-10-09-attachments-results.json).
