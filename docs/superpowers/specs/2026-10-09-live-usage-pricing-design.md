# Live usage pricing parity

Live usage enrichment must use the same local pricing precedence as reports: an existing provider cost, then user `model_pricing`, then the bundled provider/model catalogue. Unknown pricing remains absent; explicit zero and sub-cent amounts remain valid. Cache reads and writes are subsets of input and use the existing `Pricing::estimate_cost` formula.

A pure resolver in `metrics::pricing` accepts a `PriceBook`. Legacy reply enrichment and state-machine effect enrichment use injectable wrappers around that resolver, loading configured prices at their production boundary. Tests exercise both paths without mutating global configuration. Persisted costs are not repriced, replay recording gates stay unchanged, and no event or ledger schema is added.

Verification covers provider precedence, custom-model overrides, cache accounting, unknown pricing, zero and micro-costs, plus both agent-loop enrichment paths. Cargo validation is coordinated through the single shared lane. This change does not expand accounting to unrecorded auxiliary calls or claim complete cost coverage.

## Persisted cost coverage

`SessionUsageTotals` exposes `priced_entries`, `unpriced_entries`, and `coverage_unknown` alongside its existing totals. The read-only recursive tree query counts real ledger rows, including zero-cost rows as priced. Missing ledger coverage, accumulated totals exceeding ledger sums, and historical `carried_forward` rows remain unknown. The known cost subtotal is retained; completeness requires no unpriced rows and no unknown coverage. No schema or ledger writes change. This scope covers the recorded session tree, not unrecorded auxiliary calls.
