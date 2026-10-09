# Shell provider environment filtering — 2026-10-09

The maintainer requests continued roadmap work while testing the existing debug
binary. Security of shell/provider credentials is the next priority. Base commit:
`193097b174cc067e7b177933c135de5dc390de5f`. GitHub issues are disabled; this local
spec records the bounded scope before implementation.

## Problem and scope

`developer/shell.rs::build_shell_command` inherits the parent environment on
Unix and Windows. Flatpak host execution also inherits the host environment.
`resolve_login_shell_path` executes a profile-sourcing shell with inherited
credentials. The shared Developer shell tool serves both agent engines, including
bang-shell execution. Process groups provide cancellation, not OS isolation.

Remove automatic inheritance of environment keys declared secret by every
registered provider, including custom/declarative providers. Read names from
the provider registry metadata through a dedicated secret-key-name accessor; do not read secret values, keyring,
OAuth caches or stored secret files. Copy key names only (not model catalogues). Registry initialization is cached;
its lazy provider-construction/configured callbacks are not invoked by this read.
Resolve the current registered names for
each shell invocation so a refreshed custom provider is covered. Deduplicate
names and handle Windows environment-key case insensitivity.

Refresh custom providers transactionally under one write guard: clone the
registry, load into its replacement, and swap only after success. Concurrent
readers wait rather than observe absent custom keys; a failed load keeps the
previous registry intact. Catalogue cloning occurs on rare refresh, not per
shell invocation. Test failed refresh using a synthetic non-directory config path.

Preserve the remaining development environment, working directory, resolved
PATH and existing AGENT_SESSION_ID behavior. A strict allowlist would break
existing builds, toolchains and SSH workflows and requires a broader design.
No new setting or automatic bypass is introduced. Commands that depended on
inherited provider API keys must configure their credentials explicitly; this
is a deliberate compatibility change.

## Interfaces and behavior

Add a small metadata-to-environment-key helper, accepting metadata in its pure
form for tests. Shell execution obtains its snapshot asynchronously before
constructing a command. Pass that snapshot to command construction and apply
`env_remove` for the native child and `--unset-env=NAME` before the executable
for Flatpak host execution. Also clear those keys from the Flatpak launcher.
Do not apply this globally to MCP or provider subprocesses that intentionally
need credentials.

Apply the same snapshot filtering to the login-PATH probe. Keep
DeveloperClient::new synchronous: LoginPath::spawn may start an async task that
collects names before its blocking probe, retaining its existing owned handle.
The hook PATH probe currently calls resolve_login_shell_path too; adapt that
caller to obtain/pass names before the blocking call. Actual user-configured
hook commands are outside this lot.

Remove inherited BASH_ENV and ENV from execution and the PATH probe (native and
Flatpak). They can source startup commands and reintroduce credentials before
the requested command. Do not claim that this prevents shell startup files:
zsh still reads startup files, custom KAJI_SHELL implementations may differ,
and login probes deliberately source user profiles. Removing ZDOTDIR alone does
not disable .zshenv and can change user configuration unexpectedly; retain it
and document this residual behavior rather than promise startup-file isolation.

Existing timeout/cancellation/output behavior remains shared and unchanged.
No external state is added to model prompts and no replay event is needed.

## Verification

Use fake credentials only. Avoid mutating global environment in parallel tests:
spawn an isolated test process with controlled env or inspect constructed
commands for the pure parts. Verify builtin and custom metadata keys are removed,
including a registered-provider refresh and Windows case matching in pure tests.
Verify ordinary dev variables, PATH, cwd and session IDs remain available.
A real native shell and its descendant must not observe fake provider keys;
BASH_ENV/ENV fixtures must not execute their marker startup scripts. Check
Flatpak launcher removals and host unset arguments and probe construction without
requiring a Flatpak host. Parent fake credentials must remain available to a
synthetic provider; no parent environment mutation is allowed.

Run targeted tests, then the relevant crate suites and strict clippy using Hermit,
locked/offline dependencies, RUSTC_WRAPPER empty, incremental disabled, four jobs.
Format touched Rust files individually because unrelated edits exist. Use a
separate target/build destination; never overwrite
`/tmp/kaji-validation-2026-10-09/debug/kaji`, which the maintainer is testing.
If feasible, an isolated localhost provider and a real terminal check both
legacy and state-machine engines with identical shell/descendant fixtures.
Update the self-test recipe; record the retired-model HTTP 410 honestly if its
attempt remains blocked, without changing provider/model/profile configuration.

## Non-goals and limits

This filters declared provider credential environment variables; it is not an
OS sandbox or complete secret protection. Unknown/unregistered secret keys,
SSH agents, cloud credentials not declared by provider metadata, files/keychain,
profile-sourced credentials and explicit commands/configuration remain reachable
according to host permissions. No shell/filesystem/network confinement, output
redaction, MCP/hook audit, new dependencies, real-secret mutation, model change,
main merge, installed-binary replacement or competitive benchmark belongs here.
Preserve unrelated forge/UI/Git/mission-control/graphify edits.
