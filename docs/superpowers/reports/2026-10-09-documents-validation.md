# Document and suggestion validation — 2026-10-09

At the initial validation, local uncommitted CLI/TUI changes followed the maintainer's choice to display
documents inside the terminal. Existing unrelated forge, mission-control and
Git-display edits remain preserved. No commit, publication or installed-binary
replacement was performed.

## Result

The final lean CLI library suite passes **1,157 tests, zero failures, two ignored**
in 4.32 seconds. All-target lean CLI clippy with `-D warnings` passes, followed by
a successful debug build. This does not establish that all workspace or
default-feature checks pass for this lot. The earlier optimised executables,
default-feature tests and runtime baseline precede these changes.

Real terminal checks use the rebuilt debug executable, an isolated tmux server,
fresh sessions and a localhost provider fixture. Nine initial file previews make
**zero model requests**: source code, UTF-16, DOCX, ODT, PPTX, XLSX, PDF, PNG and
an oversized compressed DOCX refusal. Sparse spreadsheet addresses A1 and D1
remain visible. A long paragraph can be read to its final marker; resizing at the
end keeps the last page filled. Images remain visible at 40×24 and documents at
120×30. Unit buffer checks validate coloured half-block raster output.

Five primary chat turns verify suggestions are off by default, use the latest
chat context when enabled, request at most 256 output tokens, can be accepted
with Tab, cancel when typing, clear loading after failure, and stop after
`/suggest off`. Six suggestion HTTP attempts were observed: one successful, one
cancelled and four provider retries during one failed suggestion. One separate
memory-curation request was also observed. A suggestion
task is not necessarily one HTTP request. Ancillary ledger accounting remains
to audit. Fixture metadata is in
[documents results](2026-10-09-documents-results.json).

The updated `kaji-self-test.yaml` recipe was attempted after rebuilding. The
configured `ollama_cloud` model `deepseek-v4-flash:0731` is retired and returns
HTTP 410; the CLI exits 1. **The recipe has not passed.** No provider configuration
or credentials were changed to bypass that failure.

## Publication verification

Publication was subsequently requested by the maintainer on the tracked
`feat/kaji-init` branch. Earlier forge, mission-control and Git-display edits and
generated graph files remain local. The prepared source was extracted from the
index into a separate directory and checked independently: **1,146 CLI lean
tests passed, zero failed, two ignored** (4.30 seconds), followed by successful
all-target CLI clippy with `-D warnings`. The 11-test difference from 1,157 above
comes from the deliberately excluded earlier local edits. No tests were skipped
to obtain the smaller count. Only publication documentation changed after this
isolated verification; Rust files were unchanged. The earlier actual-terminal
and optimised runtime checks describe their original working-tree artifacts.

## Implemented behavior and budgets

`/open <path>` accepts paths with spaces and opens a local, read-only preview.
A single dedicated reader thread handles work outside the terminal event loop.
At most one job runs; the next queued request replaces older queued requests.
Request identities discard stale results after closing or opening another file.
Failed reloads preserve the previous readable snapshot. Closing the reader signals
cancellation and waits at most 100 ms; a stalled filesystem read can outlive that
wait. This avoids waiting indefinitely for a Tokio blocking-pool task on exit.

Regular-file checks refuse directories, devices and FIFOs before reading;
descriptor checks and nonblocking Unix opens cover special-file replacement.
Ordinary file symlinks remain supported. Display controls and bidi controls are
sanitised. These properties are not a complete filesystem security audit.

| Input | Limit and behavior |
| --- | --- |
| Source/text | Read at most 256 KiB; UTF-8 and BOM-marked UTF-16; binary fallback; retained display at most 256 KiB and 4,096 lines |
| DOCX/ODT/PPTX/XLSX | Input at most 8 MiB; at most 1,024 ZIP entries and 512 KiB central directory; aggregate XML at most 512 KiB |
| Office structure | At most 32 slide parts, 16 sheet parts and 4,096 shared strings; DTD/external entities refused; no extraction to disk |
| PDF | Input at most 8 MiB; `pdftotext` pages 1–20; 4-second timeout; retained output at most 256 KiB |
| PNG/JPEG/WebP | Input at most 5 MiB; at most 4 million pixels before decode; retained thumbnail at most 160×160 RGBA pixels |
| Wrapped document | One width-specific cached layout; at most 4,096 rows, explicit truncation notice; Unicode cell widths |
| Suggestions | Current recent chat, at most 4 blocks / 8 KiB; 256 requested tokens, 256 streamed chunks / 240 displayed characters; 10-second timeout |

Image decoder allocation limits are best effort, not a hard process RAM bound.
The thumbnail alone retains at most 102,400 bytes; peak decoding memory is larger.
Very large photos are refused rather than fully decoded. Images use terminal
colour half blocks; Kitty, Sixel and iTerm image protocols are not implemented.

Office previews expose extracted text. They do not reproduce page layout,
embedded images, styling, date/number formats or calculated formulas. Slide/sheet
parts are ordered by numeric part name, not by presentation/workbook relationship
order. Macros, external links and formulas are not executed. Unsupported formats
show a readable fallback card. Office/PDF `@` attachments to the model remain
pending; a local preview does not inject content into model context.

PDF conversion requires Poppler's `pdftotext` on PATH. A private bounded snapshot
prevents the converter from opening the original changing file. Arguments are
passed separately without a shell. The converter receives only PATH, with stdin,
stdout and stderr disconnected. Unix CPU/output-file limits are set; Linux also
gets a 512 MiB address-space limit. macOS rejected that address-space limit and
does not receive it. Timeout and cancellation kill and reap the child/process
group. This is **not OS isolation** of files or network access. Scanned PDFs need
OCR, which is not implemented. Only macOS execution was checked here.

Suggestions own one cancellable task; replacement, editing, a new turn and exit
cancel it. Failure clears loading. Context excludes internal thinking/system/tool
blocks. Provider streaming avoids collecting the entire completion; an individual
upstream chunk may still allocate in the provider transport before the CLI can
apply its retention limit. No core agent-loop or replay behavior changed.

## Reproduction and scope

Rust 1.96.1, macOS 27, Apple M2, 24 GiB. Hermit toolchain activated;
`RUSTC_WRAPPER=` because local sccache fails, `CARGO_INCREMENTAL=0`, four jobs,
target directory `/tmp/kaji-validation-2026-10-09`. Dependencies resolved locked
and offline. Direct image, XML and Unix libc dependencies already existed in the
lockfile; only CLI dependency references were added, with no version upgrades.
The checked debug executable SHA-256 is
`32e7817cb25d1d3408b715edaa259c123e2fb5730498ee36501c5ef613993570`.

```sh
source bin/activate-hermit
export CARGO_TARGET_DIR=/tmp/kaji-validation-2026-10-09
export CARGO_INCREMENTAL=0 RUSTC_WRAPPER=
cargo test -p kaji-cli --lib --no-default-features \
  --features tui,rustls-tls,system-keyring,update --jobs 4 --locked --offline
cargo clippy -p kaji-cli --all-targets --no-default-features \
  --features tui,rustls-tls,system-keyring,update --jobs 4 --locked --offline -- -D warnings
cargo build -p kaji-cli --bin kaji --no-default-features \
  --features tui,rustls-tls,system-keyring,update --jobs 4 --locked --offline
```

Touched Rust files were formatted individually; unrelated local edits prevented
using global `cargo fmt`. Tests cover stale requests, queued loads, reload
failure, controls/bidi, source/row budgets, ZIP expansion and directory bounds,
DTD rejection, XML entities, sparse cells, image bounds, process timeout/reaping,
environment-secret removal and cancellation. Terminal fixtures use synthetic
documents; they do not establish compatibility with every Office/PDF file or
physical high-resolution image fidelity. No new runtime/RAM benchmark, dependency
advisory audit, competitor evaluation or complete security audit ran in this lot.

Next priorities: bounded asynchronous Office/PDF model attachments, ancillary
usage accounting, shell/credential OS confinement and longer-session profiling.
Native graphics and OCR require their own measured implementation and validation.
Repository and canonical NAS roadmaps are updated; the older canonical P3 history
is preserved byte for byte. The fixture server and dedicated tmux server are
stopped. Formatting checks and `git diff --check` pass.
