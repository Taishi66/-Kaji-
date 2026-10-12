# Terminal session status validation

Status: implementation in progress; publication gates pending.
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
- UI tests, strict CLI lint, isolated debug build, terminal widths/themes and
  updated recipe: pending.
- Exact published-source gate and visual review: pending. Agent review does not
  constitute user visual acceptance.

Validation uses a separate CoW cache `/tmp/kaji-status-validation-2026-10-09`.
The user-tested previous executable remains untouched, SHA256
`d41df09fb05a6b3e4d94423cb4a795261060883af61ceb8d15625e403d542b79`.

Designs: [terminal session status](../specs/2026-10-09-terminal-session-status-design.md)
and [live usage pricing](../specs/2026-10-09-live-usage-pricing-design.md).
