# kaji

A native terminal AI agent, built in Rust. Describe what you want to do, follow
the work, and make decisions when Kaji asks.

Kaji is an independent project built from Goose. Its focus is a clear terminal
interface, a small local footprint, and useful memory across sessions.

## Start

Build from this repository with Rust 1.94.1 or newer:

```bash
cargo build --profile lean -p kaji-cli --bin kaji \
  --no-default-features --features tui,rustls-tls,system-keyring,update
```

With `just` installed, the same build is available as `just release-lean`.

Choose your model provider, then open Kaji in your project directory:

```bash
target/lean/kaji configure
target/lean/kaji
```

Once the binary is installed on your PATH, use `kaji` from any project. Interactive
terminals open the TUI automatically. You can also launch it with `kaji tui`.

Type a task or question and press **Enter**. No command is needed to start working.

| You want to… | Use |
| --- | --- |
| Discover commands | `/`, then ↑/↓ and Enter |
| See keyboard shortcuts | `/help` |
| Find a file to read or attach | `/files` or Ctrl+P |
| Read a file or document without a model request | `/open <path>` |
| Include a file in your message | `@` |
| See token usage and cost | `/cost` |
| Change colours | `/theme` |
| Change the permission mode | Shift+Tab |
| Interrupt the current task | Esc |
| Leave Kaji | `/quit` or Ctrl+C |

The terminal reader previews UTF-8/UTF-16 text and code, DOCX/ODT text, PPTX slide
text, cached XLSX cell values, PDF text, and PNG/JPEG/WebP thumbnails in colour
cells. PDF text needs `pdftotext` (Poppler) on PATH; scanned pages need OCR.
Office previews show extracted text, not page layout. Spreadsheet addresses are
retained; formulas and macros are never run. Long paragraphs reflow with the
terminal width using a bounded cached layout. Unsupported formats or exceeded
budgets show a clear notice. `/open` reads locally and does not attach or send.
`@` attachments still support text and still images; attaching Office/PDF contents
as model context is a separate pending step.

Preview reads run on a single worker. Text is capped at 256 KiB and 4,096 lines;
Office/PDF inputs at 8 MiB, Office XML at 512 KiB, images at 5 MiB and 4 megapixels.
PDF conversion has a four-second deadline; Unix additionally caps CPU and output,
and Linux caps address space. Images retain at most a 160×160 thumbnail. These budgets do not
constitute an OS security sandbox.

Next-prompt suggestions are **off by default**. `/suggest on` enables an extra
model request after replies; `/suggest off` stops it. Suggestions use the current
visible exchange with bounded context and are cancelled when you edit or start a
new turn. Failed requests may be retried by the provider client within the timeout.
They are optional; Tab accepts one into the composer for editing.

The welcome explains the current permission mode. Choose **approve** when you
want Kaji to ask before each tool call; **auto** allows it to act without asking.
Existing permission rules still apply.

The status bar keeps the permission mode visible and labels tokens, active agents
and activity. It uses standard text by default. For optional Nerd Font icons,
set `KAJI_ICONS=nerd` in a terminal with a compatible font.

Resume your last session with `kaji tui --resume`. Use
`kaji tui --resume --session-id <id>` for a specific session.

## Build variants

The lean build includes the terminal UI, API providers outside the optional AWS
group, MCP tools, sessions and memory, Rustls TLS, the system keyring and the update
command. It leaves out the optional JavaScript code-mode runtime, embedded local
inference, AWS providers, Nostr and OpenTelemetry. External providers such as
Ollama remain available. The `lean` profile enables Thin LTO and strips debug
information; its binary is separate from the full build in `target/lean/kaji`.
Thin LTO may increase build time and memory during compilation.
Build-time macros retain their symbols to avoid a Rust 1.96/macOS 27 dylib loading
bug; this exemption does not change stripping of the final lean executable.

For all default features, use `just release-binary` or:

```bash
cargo build --release -p kaji-cli --bin kaji
```

The full binary is in `target/release/kaji`. Build variants select optional
components; they do not change your saved sessions or configuration. A local
Apple M2 baseline measured about 136 MiB for lean and 266 MiB for full. Idle RAM
was about 44/49 MiB, then 105/110 MiB after the same 32 exchanges. Both started
in about 87 ms at the median in the repeated warm-start fixture. Local turn latency
was essentially equal. Speed and RAM matter more than executable size when
choosing a variant; these local measurements do not establish superiority over
other harnesses.
See the [validation report](docs/superpowers/reports/2026-10-09-terminal-validation.md)
for the exact scope, results and remaining checks.

## Development

See [AGENTS.md](AGENTS.md) for repository workflow and verification rules, and the
[terminal design](docs/superpowers/specs/2026-10-09-terminal-minimal-design.md) for
the current scope. The [roadmap](ROADMAP.md) records progress and remaining checks.
The terminal calls the Rust core in-process. A desktop client
using Tauri v2 and ACP is planned and is not implemented yet.

Kaji is licensed under [Apache 2.0](LICENSE). Original notices are preserved in
the repository.
