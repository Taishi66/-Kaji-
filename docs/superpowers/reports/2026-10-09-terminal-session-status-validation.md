# Terminal session status validation

Status: lean CLI gates and agent terminal run passed 2026-10-12; recipe, default-feature build and user visual acceptance pending.
Baseline: `40e79dc92b11ab8c05faa037ae738efa50af7253`.

## Scope and contracts

The UI separates model identity, estimated prompt context and recorded spending.
An explicit positive configured context limit is a prompt budget, not independently
verified provider capacity. Unknown capacity/budget must not be invented from a
fallback. Context assembly/token counting runs outside rendering and frequent ticks.
Persisted session-tree snapshots hydrate resumed sessions and replace displayed
totals instead of being added to live counters. This scope includes recorded
subagents, but not unrecorded ancillary inference.

Live pricing uses provider-reported cost first, then local `model_pricing`, then
the embedded provider/model catalogue. Existing cost/source are retained. Cache
read/write are input subsets and are charged once. Unknown, explicitly zero and
small positive prices remain distinct. Read-only ledger coverage counts priced
and unpriced real entries; accumulated drift, legacy fallback and historical
`carried_forward` rows retain unknown completeness. No schema or write behavior
changes. Both agent loops use the shared resolver; replay recording gates remain
unchanged.

The inspected local selection is `ollama_cloud / deepseek-v4.1-flash`, with no
matching embedded catalogue entry or local pricing override. Cost is therefore
unknown unless the provider supplies it. This report introduces no price feed,
provider/profile change or claim of full billing coverage.

Docker availability is a concise welcome summary, with explicit diagnostics on
request. Detection must be bounded and non-blocking; Docker is never started by
this lot. Reader controls prioritize actual close or focus gestures at narrow
widths, preserving existing keyboard behavior.

## Evidence so far

- Core usage first parallel run: 33 passed, one compaction assertion failed,
  2,080 filtered out; `/tmp/kaji-status-core-usage-tests.log`.
- Core usage sequential rerun after the conservative zero-cost legacy coverage
  regression: 34 passed, zero failed/ignored, 2,080 filtered out, 1.68 seconds;
  `/tmp/kaji-status-core-usage-serial-tests.log`. Both live pricing paths, cache
  accounting and coverage regressions passed. The compaction assertion also passed.
- Pricing module: 11 passed, zero failed, 0.11 seconds; root relay.
- Strict core all-target clippy: passed, 1 minute 54 seconds; root relay.
- Repeat parallel usage gate: 34 passed, zero failed, 1.48 seconds, including
  compaction; root relay session39175 exit0. The initial failure did not recur;
  its cause has not been established.
- 2026-10-12, commit `ee90e4000`, lean features
  (`--no-default-features --features tui,rustls-tls,system-keyring,update`),
  compiler cache wrapper disabled:
  - CLI all-target check: passed.
  - CLI library tests: 1,170 passed, zero failed, five ignored
    (`capture_session_status_buffers` is ignored on purpose).
  - Strict CLI all-target clippy: passed after merging two identical style
    branches in `statusbar::render_telemetry`.
  - Debug build: passed; `kaji --version` reports 1.45.0.
- 2026-10-12, agent-driven terminal run of that debug build in tmux, isolated
  `KAJI_PATH_ROOT`, keyring disabled, placeholder provider key, no inference:
  - 200, 120 and 80 columns: status line reads
    `deepseek-v4.1-flash · ctx ≈10k/? · session tokens 0↑ 0↓ · cost ?`.
  - 40 columns: `deepseek-v4.1-fl… · ctx≈10k/? tok0↑0↓ $?`; model, context,
    tokens and cost all remain.
  - Welcome shows `Docker unavailable · check Colima · /docker details`;
    `/docker details` prints the daemon error on request. Docker was not started.
  - Reader at 40 columns: focused footer `q close · Esc close · j/k scroll`;
    unfocused footer `Ctrl+O→q close`; `q` closes it.
- Not covered by that run: resumed-session hydration and long-turn refresh
  (both need real inference), light/dark theme comparison, the recipe run.
- Default-feature check fails in this environment while building the vendored
  `cpp-httplib` C++ dependency (OpenSSL `X509_get_subject_name` error); the
  cause is not established and kaji sources were not reached.
- Core `kaji` tests and clippy were not rerun on 2026-10-12.
- Exact published-source gate, recipe run and user visual review: pending.
  Agent review does not constitute user visual acceptance.

Validation uses a separate CoW cache `/tmp/kaji-status-validation-2026-10-09`.
The user-tested previous executable remains untouched, SHA256
`d41df09fb05a6b3e4d94423cb4a795261060883af61ceb8d15625e403d542b79`.

Designs: [terminal session status](../specs/2026-10-09-terminal-session-status-design.md)
and [live usage pricing](../specs/2026-10-09-live-usage-pricing-design.md).
