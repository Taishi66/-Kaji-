# Shell provider environment validation — 2026-10-09

This lot follows attachment commit `193097b174cc067e7b177933c135de5dc390de5f`.
It addresses automatic inheritance of declared provider credential environment
variables by the shared Developer shell tool. This is a limited security
property, not an OS sandbox or a complete secret audit.

## Behavior

The current provider registry supplies secret key names for each invocation,
including registered custom providers. Names are copied without model catalogue
cloning or secret-value/keyring/cache reads. Both metadata spelling and Config's
uppercase environment lookup spelling are removed. Windows environment handling
is case insensitive. Native child environments remove these keys; Flatpak host
execution receives unset arguments before its executable, while the launcher
also removes them. Removals take precedence over PATH/session overrides if a
provider explicitly declares one of those names secret.

BASH_ENV and ENV are removed too. The login-PATH probe uses the same snapshot;
its asynchronous caller obtains names before entering the blocking probe. The
hook PATH probe is adapted, while actual user-configured hook execution remains
outside scope. Custom-provider refresh now holds one write guard and stages a
replacement registry, swapping only after successful loading. Concurrent readers
cannot observe an empty custom-key window; failed loading preserves the previous
registry. Rare refresh clones the registry and can block readers during file I/O.

Both agent engines use this shared Developer tool, including bang-shell requests.
No core loop, prompt-state source or replay event kind was changed. The parent
process environment remains intact so the provider can authenticate normally.
Remaining development variables, cwd, PATH and AGENT_SESSION_ID remain available
unless the same key is explicitly declared secret by provider metadata.

Commands that previously inherited provider credentials must configure them
explicitly. No new bypass setting was introduced. Profiles and startup files can
reload credentials, especially during intentional login-profile sourcing and
custom shells; removing ZDOTDIR does not prevent .zshenv and was not added.
Unknown/unregistered keys, SSH agents, cloud credentials outside provider
metadata, files/keychain, explicit commands and MCP/hooks remain outside the
proven boundary. Shell host permissions and network access remain unchanged.

## Verification

- Shared shell unit module: **28 passed, zero failed**, including new metadata
  normalization, Flatpak/probe construction and override-collision checks.
- Controlled-process integration: **one passed, one ignored helper**, zero failed.
  The parent test launches the ignored helper explicitly with fake credentials
  and a temporary KAJI_PATH_ROOT. Real shell and descendant lack builtin/custom
  credentials and startup injection variables; dev environment/cwd/session remain.
  A newly refreshed custom key is removed, and a failed refresh retains earlier
  custom-key filtering. Parent fake credentials are unchanged.
- Hook unit module: **10 passed, zero failed**.
- Provider initialization unit module: **8 passed, zero failed**.

The initial integration fixture used unsupported KAJI_CONFIG_DIR and correctly
failed its custom-key assertion. It was corrected to KAJI_PATH_ROOT/config with
an explicit resolved-path and registered-metadata assertion. No assertion was
weakened. No real credential value was inspected, changed or printed.

Touched files are formatted individually with skip_children enabled. Two
incidental recursive rustfmt changes in unrelated provider files were restored.
Hermit/Rust 1.96.1, locked/offline dependencies, compiler wrapper disabled,
incremental disabled, four jobs. Target `/tmp/kaji-security-validation-2026-10-09`
is separate from the maintainer's active attachment binary. Its APFS clone has a
different executable inode; the original binary SHA remains
`9efec62acd30f031e2be08094dc6249c6de3527ae8bd504b087634c58d50a7fe`.

Strict clippy for core and CLI all targets passes with `-D warnings` (2m47s).
Lean CLI build passes (2m51s), using `tui,rustls-tls,system-keyring,update` with
no default features. Its debug binary SHA-256 is
`fcf9a017a810a10be0238ce51450adb990c5e3c7f9ae4870c98f866e0e410314`.
These checks and the binary include earlier unrelated local CLI edits, before
the subsequent UI lot; this lot itself changes no CLI source.

Actual terminal checks pass in both legacy and state-machine engines. A synthetic
localhost provider verifies its fake Authorization credential on all four main
requests (one tool call and one result per engine), while the real shell and
its descendant report builtin/custom provider keys and BASH_ENV/ENV absent.
The development variable remains available and both terminals complete normally.
Only controlled fake values appear in fixture records; no external model is used.
[Fixture results](2026-10-09-shell-provider-environment-results.json) retain this evidence. CLI library tests have not
been repeated because this lot changes no CLI source. The self-test recipe now
points to the isolated fake-secret integration test; the configured retired-model
HTTP 410 from the preceding lot has not been reattempted or reported as passing.
Default/full-feature/workspace and native Windows/Linux/Flatpak-host execution
are not validated by these checks.
