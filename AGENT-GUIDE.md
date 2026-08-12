# Guide agent — KAJI

Complète `AGENTS.md` (hérité de goose, à lire en premier). Pas de duplication
ici : ce fichier couvre ce qui est spécifique au fork KAJI.

## Ce qui diffère de goose

Rebrand complet effectué (commit `fc96153f9`) : 12 crates `goose*` → `kaji*`
(dirs, `Cargo.toml`, imports, `Cargo.lock`), binaire + CLI `kaji`, env vars
`GOOSE_*` → `KAJI_*` (dont `KAJI_STATE_MACHINE`), `.goosehints` → `.kajihints`,
`.goose` → `.kaji`, self-test → `kaji-self-test.yaml`. Repo GitHub :
`Taishi66/-Kaji-` (remote `kaji-origin`) ; `goose-upstream` reste pointé sur
`aaif-goose/goose` pour tirer des correctifs.

**Limites volontaires du rebrand** (ne pas "corriger" par réflexe) :
- La dépendance crates.io **`v8-goose = "145.0.2"`** (`vendor/v8/Cargo.toml`)
  reste sous ce nom — c'est le nom du crate vendeur upstream, pas un artefact
  de rename oublié.
- Les URLs externes réelles (`goose-docs.ai`, `github.com/aaif-goose/goose`,
  badges CI du `README.md`, `.github/workflows/docs-update-cli-ref.yml`)
  restent inchangées — elles pointent vers de la doc/CI upstream réelle, pas
  vers KAJI.
- `.kajihints` (racine et `documentation/`) a été renommé mais son **contenu**
  décrit encore l'ancienne topologie goose (`kaji-server`, `kaji-bench` —
  crates qui n'existent plus dans ce workspace). Ne pas s'y fier pour la
  carte des crates ; se référer à `crates/` directement.

## Sous-système mémoire

- **Moteur** : `crates/kaji-core/src/memory.rs` — store SQLite FTS5
  (`rusqlite`), recall BM25, `anchored_view`, budget `BUDGET_MIN`/`BUDGET_MAX`
  (0.6/0.7). Zéro dépendance hors `rusqlite`.
- **Pont vers le noyau agent** : `crates/kaji/src/kaji.rs` — `SessionMemory`
  (store partagé `shared.db`), `splice_memory_block`/`ingest_turn` appelés à
  l'identique dans `agents/agent.rs` (legacy) et
  `agents/state_machine/ops_llm.rs` (state machine).
- **CLI** : `kaji memory list` (`--session`, `--limit`, `--format text|json`)
  et `kaji memory clear` (`<session>` ou `--all` — l'un des deux est
  obligatoire, anti-effacement accidentel). Implémentation :
  `crates/kaji-cli/src/commands/memory.rs`.
- **Isolation tests** : `KAJI_MEMORY_DIR` redirige le store hors du data dir
  réel de l'utilisateur — à positionner dans tout test qui touche
  `SessionMemory`/`kaji memory`, sous peine de polluer (ou lire) le vrai
  store partagé.

## Pièges connus

- **Rebuild du binaire CLI** : `cargo build -p kaji-cli --bin kaji` est
  nécessaire pour embarquer un changement du pont mémoire ou de tout code
  dans `crates/kaji`. `cargo build -p kaji` seul ne régénère **pas**
  `target/debug/kaji` (constaté au run E2E mémoire du 2026-08-08).
- **8 échecs préexistants** sur `cargo test --package kaji --lib` (1671
  verts / 8 en échec, re-vérifiés à plusieurs HEAD successifs) : 2 tests de
  compaction, 4 `gcpauth`, 1 snapshot `prompt_manager`, 1 cutoff. Non liés au
  travail mémoire — ne pas tenter de les "réparer" en passant, et ne pas s'en
  inquiéter s'ils réapparaissent identiques après un changement sans rapport.
- **Parité legacy/state-machine** : toute modification touchant la boucle
  agent (prompt, mémoire, compaction, tool-calling) doit être répercutée dans
  `crates/kaji/src/agents/agent.rs` **et**
  `crates/kaji/src/agents/state_machine/` — la bascule est
  `KAJI_STATE_MACHINE=1` (`agents/state_machine/mod.rs::enabled()`). Un
  changement dans un seul chemin est un bug de parité, pas une simplification.
- **Jamais écraser un binaire vivant en place** — `cp`/copie directe sur un
  exécutable en cours d'exécution provoque un SIGKILL macOS ("Code Signature
  Invalid"). `just install` fait déjà l'unlink correct
  (`rm -f` puis `cp -p`) ; ne pas le contourner.

## Workflow

- Branche de travail actuelle : `feat/kaji-init`.
- `cargo fmt` et `cargo clippy --all-targets -- -D warnings` obligatoires
  avant tout commit — jamais skip.
- Tests par crate ciblé (`cargo test -p kaji-core`, `cargo test -p kaji-cli`,
  etc.) plutôt que la suite complète pendant l'itération ; `cargo test -p
  kaji --test <nom>` pour un fichier précis sous `crates/kaji/tests/`.
- `just release-binary` build le CLI en release (`cargo build --release -p
  kaji-cli --bin kaji`) — plus de copie vers un desktop depuis le retrait
  d'`ui/desktop`. Pour un binaire installé localement (unlink + codesign),
  utiliser `just install`.
