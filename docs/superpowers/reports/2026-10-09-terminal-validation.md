# Terminal validation — 2026-10-09

This report records the earlier minimal-terminal and runtime baseline. The later
document/suggestion lot is covered by the
[document validation report](2026-10-09-documents-validation.md); the optimised
artifacts and runtime measurements below do not include that later lot.

Local, uncommitted changes. This report includes the minimal terminal work and
the bounded Markdown cache. Existing forge, mission-control and Git-display
changes remain in the working tree; they are not attributed to this session.

## Latest result

The final source passes **1,130 lean CLI library tests**, with zero failures and
two ignored, plus lean CLI clippy on all targets with `-D warnings`. Five targeted
core security integration tests pass. Eight real-binary checks validate headless
failure/success reporting in both agent engines. Actual terminal checks validate
attachments, readable approvals, denial, streaming cancellation, themes and resume.

The configured self-test recipe cannot pass: its model returns HTTP 410. The CLI
now reports that failure with exit 1. The final optimised lean artifact is built,
and its eight headless subprocess checks pass again. The default-feature CLI
library suite also passes: **1,130 passed, zero failed, two ignored** in release
mode. These are the same CLI tests in two configurations, not 2,260 unique tests.
The final full artifact is also built and passes the same eight headless checks.
Matched runtime measurements are complete. The sections below retain the dated
intermediate results; they are not additional tests to add to the final count.

## Honest comparison and limits

Native executable sizes measured on this Mac; Kaji rows reflect the final builds:

| Executable | Version | Bytes | MiB |
| --- | --- | ---: | ---: |
| Kaji lean | 1.45.0 | 142,611,424 | 136.00 |
| Kaji full | 1.45.0 | 279,140,304 | 266.21 |
| OpenCode | 1.18.35 | 144,306,528 | 137.62 |
| Claude Code | 2.1.295 | 239,695,888 | 228.59 |
| Codex | 0.162.0 | 246,508,160 | 235.09 |
| Local Pi Rust port (`pi_agent_rust`) | 0.1.22 | 18,455,504 | 17.60 |

The maintainer subsequently clarified that speed, responsiveness and RAM
efficiency matter more than executable size. The matched local runtime fixture
below establishes a small RSS saving, with essentially equal local turn latency.

These are executable sizes, not total installation footprints, runtime RSS or
speed benchmarks. Builds, bundled features and versions differ. The installed
Pi is a Rust port; its size must not be attributed to the original TypeScript Pi.
Kaji lean is roughly the same size as OpenCode, 40.5% smaller than Claude Code
and 42.1% smaller than Codex. Full Kaji is larger than all three. Rust alone does
not establish a footprint advantage.

My assessment: Kaji has a useful basis for a clear, extensible terminal with
provider choice, sessions, memory and workflows. This iteration improves concrete
friction points and bounded rendering work. It does **not** establish superiority
in task success, RAM, token cost or security. Two agent loops, large UI modules
and the broad default dependency graph make changes and validation expensive.
The measured cache gain decreases with longer history because the whole history
is still wrapped each frame.

[Original Pi](https://github.com/earendil-works/pi/tree/main/packages/coding-agent)
prioritises a small, extensible terminal core. Its design is a useful simplicity
reference; optional extensions and isolation policy must be assessed separately.
[OpenCode](https://opencode.ai/docs/) already provides multiple providers and
interfaces, with [granular permission rules](https://opencode.ai/docs/permissions/).
Those permission rules should not be confused with OS isolation.

[Claude Code's sandboxed Bash tool](https://code.claude.com/docs/en/sandboxing)
supports OS-enforced filesystem and network boundaries when enabled.
[Codex documents OS-enforced sandbox boundaries](https://learn.chatgpt.com/docs/sandboxing)
separately from approval decisions. Kaji's reviewed native shell path does not
provide that boundary today. This is a concrete security gap; no product is
being declared universally secure by this comparison.

Priorities before a competitive claim: sandbox and credential confinement;
finish the single-loop migration with replay/parity checks; narrow default
dependencies; measure matched cold startup and idle/long-session RSS; then run
the same coding tasks with the same model and compare success, latency and cost.

## Matched runtime measurements

Apple M2, eight logical CPUs, 24 GiB RAM, macOS 27. Existing applications stayed
open; before this run macOS reported about 3.8 GiB of used swap. All session-owned
compilation and render-timing work finished first. This is a local baseline,
not a dedicated benchmark machine or a competitor runtime comparison.

Both final binaries used the same isolated OpenAI-compatible localhost fixture,
80×24 terminal, light theme, approve mode and default legacy engine
(`KAJI_STATE_MACHINE=0`). Fresh config/session/memory stores per sample. Three
alternating samples per variant, 32 turns each (192 turns total). Each answer
contains the same header and 60 repeated Markdown paragraphs with Unicode,
ending in a unique turn marker. The fixture also correctly answers ancillary
streaming/nonstreaming calls: **192 suggestions and six memory curations**. Curations
return `[]`; suggestions return a fixed short sentence. Request counts were
verified: exactly 192 primary turns and no unknown calls. No external model,
model process or tool execution. The server produced no request-handler errors.

Startup runs from launching the tmux command until the usable welcome is visible;
it includes shell/tmux overhead and 20 ms polling. RSS sums the pane process and
its children: after 0.5s idle and 1s after the 32nd answer. Turn latency runs from
starting input injection to the complete answer being visible without a spinner;
it includes request construction, localhost transport, rendering and polling.
It does not isolate typing latency or model generation time.

| Metric | Lean | Full |
| --- | ---: | ---: |
| Warm startup median, 3 samples | 86.83 ms | 86.02 ms |
| Startup range | 86.44–95.46 ms | 85.79–87.79 ms |
| Idle process-tree RSS median | 44.47 MiB | 49.00 MiB |
| RSS median after 32 answers | 105.20 MiB | 109.84 MiB |
| Local turn latency median, 96 turns each | 141.65 ms | 141.66 ms |
| Local turn latency p95, nearest lower rank | 205.57 ms | 207.41 ms |

Per-sample turn medians: lean 142.80 / 143.06 / 138.56 ms; full 141.11 / 139.95 /
142.82 ms. Cold-machine/first-executable-launch startup is not established.

Two preliminary runs were discarded: the first fixture raised errors on ancillary
calls, and the second assumed suggestions were nonstreaming. The actual provider
also streams those calls. Both failures biased background load. The corrected
fixture handles both response formats and distinguishes main/ancillary calls by
their system instructions. New sample roots prevent reuse of previous sessions
or stores. Earlier announcements of 2.39s first-full startup and other preliminary
statistics came from that invalid protocol; they are not retained as the baseline.

Lean retains about 4.5–4.6 MiB less RSS in this fixture (9.2% at idle, 4.2% after
history). The latency distributions are very close; no meaningful runtime speed
advantage is established. Binary size almost halves, while RSS differs only
slightly. This supports the maintainer's priority: choose runtime behavior and
needed features, not executable size alone.

The roughly 60.7 MiB increase between idle and 32 turns is shared by both
variants. Lazy tokenizer
initialisation exists in `token_counter.rs`, but attributing this increase to
it requires allocation profiling. Peak RSS, much longer histories, input latency,
tool workloads, state-machine runtime performance and competitor RSS are unmeasured.

The request count exposes another efficiency priority: next-prompt generation
uses the active model after every clean turn, despite being outside the input
critical path. This fixture therefore makes roughly twice as many model requests
as primary turns. Request count is not token cost or doubled latency. Make
suggestions optional and budget/cancel ancillary work; audit its usage-ledger
coverage before claiming token-cost efficiency.

Source review also identifies a suggestion-context bug: `event_loop` receives an
owned initial `Conversation` (a `Vec<Message>`), never updates it, then passes
that snapshot to `suggest_next_prompt` after each turn. New sessions therefore
suggest from the empty starting context; resumed sessions keep their original
recent window. Correct this with a tested current snapshot in the follow-up.
This finding is not presented as a fix delivered in this batch.

[Raw samples](2026-10-09-runtime-results.json) retain all per-turn latencies and
load averages. Fixture scripts remain under
`/tmp/kaji-terminal-fixture-2026-10-09/{perf-server.py,measure-runtime.py}`.

## Automated checks

Rust 1.96.1, Apple Silicon, macOS 27, Apple clang 21.0.0. Hermit activated.
`RUSTC_WRAPPER=` bypasses the failing local sccache service.
`CARGO_TARGET_DIR=/tmp/kaji-validation-2026-10-09`, `CARGO_INCREMENTAL=0`, four jobs.

```bash
cargo test -p kaji-cli --lib --no-default-features \
  --features tui,rustls-tls,system-keyring,update -j 4 -- --quiet
```

Result: **1,125 passed, zero failed, two ignored**. The first run exposed one
outdated assertion that the mode word should disappear at rest. Updated it to
assert the requested persistent label and absence of duplication in the header.

Cache tests cover exact source replacement, width/theme changes, history
truncation, oversize rejection, byte/count budgets and repeated history scans.
Rendered buffers, styles, overflow and turn positions match cold rendering for
Markdown, tables, Unicode, streaming, resizing and theme changes.

## Render timing

```bash
cargo test -p kaji-cli --lib --no-default-features \
  --features tui,rustls-tls,system-keyring,update -j 4 \
  measure_chat_render_cache -- --ignored --nocapture
```

Synthetic Markdown answers, 80×24 TestBackend, **debug build**. Full chat draw,
including wrapping and terminal diffing; 200 frames per variant, alternating
order over four passes. Cache disabled in the baseline, avoiding insertion work.

| History | Without cache | With cache | Render-time reduction |
| --- | --- | --- | --- |
| 32 answers | 9.340 ms/frame | 3.975 ms/frame | 57.4% |
| 128 answers | 28.074 ms/frame | 23.045 ms/frame | 17.9% |

This is one local timing run, not a release benchmark or an application-wide
speedup. The final renderer still wraps the entire history; older answers beyond
the retention budget still need rebuilding. RSS, startup, typing latency, binary
size and competitive task success remain separate acceptance work.

The same comparison was repeated on the final default-feature **release** test
binary, after compilation finished, using the same 80×24/200-frame protocol:

| History | Without cache | With cache | Interpretation |
| --- | --- | --- | --- |
| 32 answers | 0.711 ms/frame | 0.277 ms/frame | 61.0% reduction in this workload |
| 128 answers | 2.734 ms/frame | 2.725 ms/frame | Essentially unchanged; no material gain established |

The short-history release gain is real in this synthetic fixture. The earlier
debug gain for 128 answers must not be presented as a release result. Wrapping
the whole history remains a scaling limitation; this cache is not sufficient to
claim faster long sessions. The timing test passed and is separate from the
1,130 functional tests.

## Build environment findings

The existing `target/debug/deps` contains **893,903 entries**; enumeration of it
and `target/release/deps` took 38.85 seconds. The first build progressed slowly.
Validation moved to an isolated directory; old artifacts were preserved.

The initial fresh build failed loading the stripped SQLx proc-macro dylib with
`mis-aligned LINKEDIT string pool`. An explicit dev override for `sqlx-macros`
with `strip = "none"` fixes the test build. Lean build-time dependencies are
exempted too; final executable stripping stays enabled. This matches the
[Rust bug report](https://github.com/rust-lang/rust/issues/157750) and uses
[Cargo profile overrides](https://doc.rust-lang.org/cargo/reference/profiles.html).

## Checkpoint — paused on request

The maintainer requested a pause on 2026-10-09. All session-owned build and
preview processes were stopped. Both optimised builds exited 143 deliberately;
their remaining compilation work is pending, not a reported source failure.

- Latest source, including status-bar spacing: **1,125 tests passed**, two ignored;
  lean CLI clippy on all targets with `-D warnings` passed.
- Lean-feature debug executable built at `/tmp/kaji-validation-2026-10-09/debug/kaji`.
  This executable predates the final spacing change and needs rebuilding.
- Actual tmux startup and palette inspected at 80×24 and 40×24. No model prompt was
  submitted; the preview used a temporary config and a localhost endpoint.
- Remaining: optimised builds, default-feature/workspace checks, wide/light
  terminal review, approvals, resume/attachments/interruption, self-test recipe,
  RSS/startup/binary-size baselines. The self-test recipe has **not run**.

To resume, activate Hermit, bypass sccache and use four jobs:

```bash
source bin/activate-hermit
export RUSTC_WRAPPER= CARGO_INCREMENTAL=0
CARGO_TARGET_DIR=/tmp/kaji-validation-2026-10-09 cargo build --profile lean \
  -p kaji-cli --bin kaji --no-default-features \
  --features tui,rustls-tls,system-keyring,update -j 4
cargo build --release -p kaji-cli --bin kaji -j 4
```

The full build uses the existing release cache, not the crowded debug cache.
Isolated preview data remains at `/tmp/kaji-terminal-preview-2026-10-09`; the
session-specific tmux server `kaji-review-20261009` has been stopped.
No commit, push or replacement of an installed binary has been performed.

## Resumption — 2026-10-09

The maintainer requested resumption. The optimised lean build completed successfully
in 6m52s using the preserved compilation cache. Artifact:
`/tmp/kaji-validation-2026-10-09/lean/kaji`, **142,624,752 bytes** (136.02 MiB),
version 1.45.0. The full optimised build completed in 11m31s with two jobs:
`target/release/kaji`, 279,113,376 bytes (266.18 MiB). These initial artifacts
precede the final fixes; final build results are recorded below.

The configured MCP transport is closed. The canonical roadmap remains reachable
via SSH, and the `shosoin-brain` CLI can call the same MCP server over a fresh
SSH transport (omit `--nas` when invoking it from this Mac).

Interactive verification uses a deterministic local SSE provider and isolated
config/project in `/tmp/kaji-terminal-fixture-2026-10-09`. This exercises the actual
TUI and tools without external model requests. It is separate from the self-test
recipe and does not evaluate model quality.

## Resumption findings and corrections

- File attachment verified through the real TUI: the localhost provider received
  the marker from `@notes.txt`, including Unicode. Tab/Enter completion previously
  kept selecting the same file; completion now suppresses the dropdown for a
  selected file, so the next Enter submits. Directory navigation stays open.
- Tool approval and refusal verified in approve mode. The isolated persisted
  tool responses contain stdout `KAJI_PREVIEW_OK` for approval and an error-marked
  declined response for denial. No permanent or session grant was selected.
- The compact approval panel clipped a simple grant on 80×24. Its default area
  is larger; new render tests require the command identifier and deny/always
  choices at 80×24 and 40×24 without opening detail. Existing anti-masking tests
  remain in the CLI suite.
- Actual mode change to smart and theme change to light inspected at 120×30.
- After use, one RSS sample: lean 96,560 KiB; full 96,912 KiB. Sessions had different
  histories. This is descriptive, **not** a controlled RAM comparison or evidence
  of a memory reduction. The matched binary-size comparison before final fixes
  is 136.02 vs 266.18 MiB, a 48.9% disk reduction.
- CLI source at this intermediate stage: **1,129 tests passed, zero failed, two ignored**. Lean CLI
  clippy on all targets with `-D warnings` passed after the headless-error fix.
- Targeted security integration tests: **5 passed** (Git fsmonitor/bare-repository
  protections and denial precedence). These are existing protections verified
  on this tree, not new implementations or a complete security audit.

The self-test recipe was attempted with the configured provider, `basic` phases,
`quick` depth, eight-turn cap and `/tmp/kaji-self-test-2026-10-09` workspace.
The provider returned HTTP 410: the configured `deepseek-v4-flash:0731` model was
retired. **The recipe has not passed.** The pre-fix command exited zero despite
this typed error. The CLI now rejects a final typed provider error and stream
errors before emitting a headless success result. Tests cover terminal failures,
recovery after an earlier failure and ordinary text discussing errors. Both
agent engines keep their existing behavior; they share this CLI consumer.
The configured model and credentials were not changed.

## Final source verification

After the typed metadata filter: **1,130 CLI library tests passed**, zero failures,
two ignored. Resume/restore tests retain genuine user text containing the same
`<turn-context>` tags and leave the persisted conversation intact.

The rebuilt debug binary was checked against a local provider returning HTTP 410:
legacy and state-machine paths both exit **1** for text, JSON and stream-JSON,
without a completed result. Both still exit **0** and emit completion on a successful
response. Eight subprocess checks passed. The real self-test recipe was retried
with the configured provider and isolated `KAJI_MEMORY_DIR`: HTTP 410 remains,
now correctly **exit 1**. This is a failed provider setup, not a passing self-test.

Actual terminal checks on the rebuilt debug binary cover Tab→Enter attachment,
readable grants at 80×24 and 40×24, deny/allow, immediate Esc during streaming,
light theme at 120×30 and session resume without repeated onboarding. The first
resume capture exposed internal context blocks; the typed metadata filter fixes
that display. The final rebuilt binary replay check passed: internal context tags/time are absent, user exchanges remain. Captures are under `/tmp/kaji-terminal-fixture-2026-10-09/final-*.txt` (temporary artifacts).

Security review of the native shell path: `build_shell_command` invokes a host
shell directly; `configure_subprocess` isolates process groups and handles lifecycle,
not filesystem/network access. The shell inherits environment variables. Permissions,
anti-masking and redaction are useful controls, but this path is not an enforced
OS sandbox. Sandbox boundaries, credential confinement, hostile-output budgets,
MCP/egress and dependency review are explicitly remaining security work.

Final lean CLI clippy on all targets with `-D warnings` also passed after the metadata filter.

The final optimised lean build completed in 18m24s while the default-feature test
dependencies compiled concurrently. Artifact: 142,611,424 bytes (136.00 MiB).
All eight headless subprocess checks pass on this artifact too. This build
duration is descriptive of the concurrent local validation, not a cold-build
comparison between products.

The default-feature CLI library suite completed successfully in release mode:
1,130 passed, zero failed, two ignored, tests executed in 0.82s. Cargo's first
build of this test graph took 69m06s on the locally loaded machine. This is
compilation time, not harness execution latency. Remaining workspace-wide tests
and clippy are not claimed as complete; no release/merge is being performed.

The final full release executable completed in 12m19s. Final artifacts:

| Variant | Bytes | MiB | SHA-256 |
| --- | ---: | ---: | --- |
| lean | 142,611,424 | 136.00 | `9736f6e12d2d1000522c25c7bdd4e1c0df9bbc0e01562c732bfc40ac57cb1e0e` |
| full | 279,140,304 | 266.21 | `1ccb8ed3cd899c00af0bb7d3e2e23eec42877d7343c250421f16fcf24c7c628d` |
