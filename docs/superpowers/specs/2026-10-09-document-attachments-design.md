# Terminal document attachments — 2026-10-09

The maintainer requests continued roadmap work on code, chat and documents,
prioritising terminal clarity, runtime efficiency and verified security properties.
The preceding lot is published as `3b9e3189a` on `feat/kaji-init`.

## Scope

An explicit `@file.docx`, `@file.odt`, `@file.pptx`, `@file.xlsx` or `@file.pdf`
adds extracted text to the existing user-message attachment envelope. `/open`
remains a local preview. Reuse the bounded, non-executing document readers;
do not add dependencies, macros, formula evaluation, OCR or native image protocols.
Quoted mentions and finder/reader attachment gestures support filenames with
spaces. Paths in attachment attributes are escaped.

Keep existing per-file 64 KiB and combined 256 KiB text budgets, images with
their separate existing caps. Limit references per submission and report refused
or truncated files visibly. PDF's first-20-page extraction and Office layout/order
limitations remain explicit. Preview strings and untrusted document contents are
ordinary user-message data, not system instructions or a new external state source.

## Preparation and cancellation

Move mention expansion for normal submission, queued steering and the first
goal work prompt out of the terminal event loop into one dedicated worker. A
bounded queue prevents repeated Esc/new submissions from creating unbounded
threads. A dropped preparation signals cancellation; PDF conversion already
observes that signal and reaps its child. An entire preparation has a deadline;
stalled filesystem reads may outlive cancellation on that single worker.

Represent pending attachment preparation separately from pending `Agent::reply`.
Only a successful, still-current preparation can construct/send the model message.
Esc/steering/goal clear/exit drop the pending preparation. Notices and image
placeholders are applied on the event loop only after that preparation resolves,
so cancelled work cannot publish stale notices or start a model request.
Return an explicit preparation failure if the bounded worker queue is full.
An empty or mention-free prompt should not require file I/O.

Both agent engines receive the same fully resolved ordinary user message.
No change to either core agent loop or prompt injection/replay event kinds is
intended; existing message persistence/replay stores the actual attachment text.

## Verification

Tests cover Office extraction in attachments, truncation/global budgets, malformed
archives, DTD refusal, special files, quoted paths and literal shell characters,
worker cancellation/replacement/queue bounds and no stale preparation results.
Check shared submission behavior for ordinary chat, steering and goal first turn.
Use a synthetic localhost provider in a real terminal to inspect the exact user
message, verify responsive input and Esc during slow PDF conversion, and verify
zero model requests after cancellation. Run both engine toggles for attachment
delivery; run lean CLI tests, strict clippy and build once after the lot stabilises.
Update the self-test recipe and attempt it after rebuilding; the configured
retired provider model may still prevent a successful recipe run.

Keep unrelated local forge/mission-control/Git edits intact. Record current
publication state and the distinction between published and local test counts
in the repository and canonical roadmaps. No competitive performance or complete
security claim follows from these checks.
