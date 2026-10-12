# Terminal session status and reader controls

Status: approved for implementation by root and Astra. Baseline: HEAD
`40e79dc92b11ab8c05faa037ae738efa50af7253`; pre-existing TUI changes are
preserved separately in `/tmp/kaji-status-baseline-2026-10-09`.

## Problem

The user screenshots show raw Docker connection errors occupying the welcome
screen and reader controls clipped before the useful closing action. The current
status bar drops the model before shortening the project path, changes token
scope implicitly between a running turn and idle session, and formats tiny known
costs as `$0.00`. Session context consumption is absent.

## Presentation

Keep the existing minimal Kaji identity and theme palette. Divide information by
purpose: keep the brand/session header and use a compact two-line status area:
project/Git, permission mode and activity on the first row; actual model, context,
stable session spending telemetry on the second. Use plain English labels;
accent is reserved for actionable controls and context pressure. No graphs,
animation, extra empty row or additional persistent pane.

At 120/200 columns show the actual model, `ctx ≈used/limit`, stable session token
input/output counts, estimated cost and project/branch changes. At 80 shorten
project paths and optional Git details before removing model/context/spending.
At 40 use compact model/context/usage/cost on the telemetry row, with project
basename and compact mode on the first. At very small heights collapse optional
status details before sacrificing the usable composer. Overflow choices
are deterministic, cell-width aware, and never hide uncertainty by showing zero.
The full model and explanatory report remain available in existing commands.

Context is an estimate of the current prompt, not cumulative session tokens.
A positive explicit ModelConfig.context_limit is a configured prompt budget,
not verified LLM capacity. The default harness fallback is not advertised as
known capacity; an absent explicit budget displays `?`. Compact context percentages refer to that configured budget, as explained in
`/help` and `/context`. Unknown capacity,
pending refresh and unavailable reports have explicit distinct states. Refresh
the cached report asynchronously on session/model transitions and completed
turns, coalescing requests and rejecting stale session results. Never assemble
prompts/tokenize from render or frequent ticks; retain the last completed estimate
while a new report is pending and mark its freshness honestly.

Token labels have one stable scope: hydrate existing persisted session-tree
aggregates on resume, and announce inclusion of subagents in the explanatory
report; compact `tok` denotes the recorded session tree. Refresh coherent ledger snapshots; never add cumulative ledger totals
to already-counted live usage or create a new global spending ledger. During
long turns, Usage events trigger a separate lightweight ledger read with one
request in flight and a 750 ms minimum interval; a dirty flag retains any event
arriving during the read. This task never computes context. Cost uses configured overrides before
the bundled catalogue, records coverage, and marks an incomplete known subtotal
as partial. Unknown price is `cost ?`; small positive costs retain meaningful
decimal precision. No new external price feed or provider/model changes.

## Reader and diagnostics

Reader footer prioritizes `q close` before optional scroll/edit/reload/attach
actions. If unfocused, prioritize `Ctrl+O focus` so `q` cannot be mistaken for a
composer/global shortcut. Show `Esc tree` only when an explorer exists; otherwise
Esc also closes. Keep real keyboard behavior unchanged, and update `/help` so
closing versus returning to the tree is explicit. Narrow layouts preserve the
closing/focus controls rather than clipping a long title.

Welcome shows one short Docker availability summary, e.g. `Docker unavailable ·
/docker for details`, with no raw socket/stderr dump. The explicit diagnostics
command provides concise cause/action plus optional raw details. Detection is
bounded and non-blocking. This lot never starts/installs Docker or changes its
configuration automatically.

## Verification and limits

Test widths 40/80/120/200 with long Unicode models/paths, known/unknown/pending
context, resumed/live usage, complete/partial/unknown prices, tiny positive costs,
active tools, and Git changes. Assert visible labels agree with the data scope,
no invented zero, width bounds and deterministic degradation. Test stale async
results after session switches and no report work on idle rendering.

Reader tests cover focused/unfocused, explorer/no explorer, q closure, Esc return,
Ctrl+O focus and minimum-width discovery. Docker fixtures cover stopped Colima,
absent socket, missing executable, success and timeout without changing the host.
Capture the actual terminal at all four widths and review zen/light/gruvbox.
Run the updated self-test recipe after a fresh build in a new target directory;
never overwrite the potentially running previous validation binary.

No virtualization, theme redesign, pricing internet research or unrelated dirty
Forge changes. Agent reviews do not constitute user visual acceptance.
