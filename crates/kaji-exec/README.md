# kaji-exec

Standalone process execution utilities: process groups, kill, `HeadTailBuffer`, background process pool.

This crate is vendored from `ante-exec` (AntigmaLabs/ante, Apache-2.0, unmaintained upstream). The code was imported verbatim, adapted to the kaji workspace (edition 2021, workspace dependencies), with attribution retained under the same Apache-2.0 license:

- HeadTailBuffer — bounded streaming buffer preserving head + tail with an omission marker
- ProcessPool — registry of long-running background processes (spawn → poll → stdin → kill)
- ProcessHandle / OutputReceiver — process lifecycle and broadcast output
- process_group / subprocess — process-group isolation, group kill, `run_with_timeout`

See the upstream repository: <https://github.com/AntigmaLabs/ante> (license Apache-2.0).