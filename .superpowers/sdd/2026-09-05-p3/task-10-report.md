# Task 10 — Hooks de cycle de vie (S6)

## Point de départ : le module existait déjà

`crates/kaji/src/hooks/mod.rs` portait déjà une infrastructure de hooks complète,
sourcée des **plugins** (`<plugin-root>/hooks/hooks.json`, spec Open-Plugins), avec
11 événements, matchers regex, timeout, spawn `sh -c` et payload JSON sur stdin.
Les 6 événements de S6 y avaient déjà leur point de branchement dans les deux boucles.

La tâche n'était donc pas de créer le module mais de lui ajouter ce qui manquait :

| Manquant S6 | Livré |
|---|---|
| Source config user + projet | `crates/kaji/src/hooks/config.rs` |
| stdout injecté dans le prompt | `HookManager::emit_capturing` + branchement `Agent::reply()` / `with_post_hooks` |
| `pre_tool_use` : exit ≠ 0 bloque (pas seulement exit 2) | `deny_reason(_, any_non_zero)` |
| Timeout fail-closed sur `pre_tool_use` | `TIMEOUT_DENIAL` dans `emit_blocking` |
| Événement `turn_end` | `HookEvent::TurnEnd`, émis dans l'enveloppe `reply()` |
| Replay : kind `hook_output` capturé + servi, zéro ré-exécution | `HookManager::with_replay` + `record/cursor/source` |
| Gate d'activation des hooks projet | `config::project_hooks_enabled()` |

## Sites de branchement par événement

| Événement | Site | Partagé / double | Fichier |
|---|---|---|---|
| `session_start` | `Agent::reply()` (enveloppe) | **partagé** | `agents/agent.rs` `apply_prompt_hooks` |
| `user_prompt_submit` | `Agent::reply()` (enveloppe) | **partagé** | idem |
| `turn_end` | `Agent::reply()`, bras de fin propre du flux | **partagé** | `agents/agent.rs` |
| `pre_tool_use` | `Agent::dispatch_tool_call` + `ToolExecutionOperation::dispatch_tool_call` | **double branchement, sémantique partagée** (`HookManager::emit_blocking`) | `agent.rs`, `ops_toolcalling.rs` |
| `post_tool_use` | `Agent::with_post_tool_hook` + `ToolExecutionOperation::with_post_hooks` | **double branchement, sémantique partagée** (`emit_capturing` + `hooks::append_tool_feedback`) | idem |
| `session_end` | `KajiSession` (CLI) ×2 | inchangé, hors boucles | `kaji-cli/src/session/mod.rs` |

### Ce qui a bougé pour obtenir le site partagé

`session_start` et `user_prompt_submit` étaient émis **deux fois**, en fire-and-forget :
`agent.rs` (legacy, ~l. 2700) et `StateMachine::emit_entry_hooks` (SM). Les deux ont été
**supprimés** au profit d'un appel unique dans `Agent::reply()`.

C'est le seul point qui satisfait les trois contraintes S6 à la fois :
- les deux boucles y convergent (`reply()` → `reply_impl` → legacy ou `reply_with_state_machine`) ;
- il précède la persistance du message d'ouverture, donc le prompt réécrit est celui que
  la conversation garde **et** celui que le journal porte sous `message` (critère d'acceptation) ;
- il précède `reply_impl`, donc le tour envoie au modèle la version réécrite.

Conséquence sur les tests : la moitié `SessionStart`/`UserPromptSubmit` de
`state_machine/tests/hooks_lifecycle.rs` ne pouvait plus passer (le harness `test_pipeline`
pilote `StateMachine` directement, pas `Agent::reply()`). Elle a été retirée de ce fichier
— renommé `pre_tool_hooks_block_at_the_tool_boundary` — et **remplacée par une couverture
plus forte** : `crates/kaji/tests/hooks_lifecycle_test.rs` joue chaque cas sur les deux
boucles via l'agent réel. Net : la parité est mieux prouvée qu'avant.

## Payload stdin exact

Un objet JSON, une ligne, borné à ces champs (les `Option` absents ne sont pas sérialisés) :

```json
{
  "event": "PreToolUse",
  "session_id": "…",
  "matcher_context": "developer__shell",
  "tool_name": "developer__shell",
  "tool_input":  { "command": "rm -rf /" },
  "tool_args":   { "command": "rm -rf /" },
  "tool_output": { },
  "message": "le prompt de l'utilisateur",
  "prompt":  "le prompt de l'utilisateur",
  "last_assistant_message": "…",
  "working_dir": "/chemin/du/projet"
}
```

- `prompt`/`tool_args` sont les noms de la spec S6, `message`/`tool_input` ceux des plugins
  Open-Plugins déjà en production. Les deux paires portent la même valeur : un script écrit
  contre l'une ou l'autre fonctionne, et aucun plugin existant ne casse.
- **Bornage** : rien de la conversation, de la config, des secrets ou de l'environnement de
  kaji n'entre dans le payload. Un hook qui a besoin de plus le lit lui-même, sous l'identité
  de l'utilisateur.
- Env du processus hook : `PLUGIN_ROOT` (racine du plugin, ou dossier de config / racine
  projet pour un hook de config), plus le `PATH` de login si `use_login_shell_path`.

## Sémantique

| Cas | Comportement |
|---|---|
| exit 0, stdout non vide, `session_start` / `user_prompt_submit` | stdout préfixé au message utilisateur (un bloc de texte par hook, dans l'ordre des règles) |
| exit 0, stdout non vide, `post_tool_use` | stdout ajouté au résultat d'outil (bloc de texte en fin, le résultat réel reste intact devant) |
| exit ≠ 0 sur `pre_tool_use` | appel **bloqué**, stderr = raison rendue au modèle |
| exit 2 ou `{"decision":"block"}` sur `Stop` | inchangé (contrat plugins préservé) |
| timeout `pre_tool_use` | **fail-closed** : appel bloqué, raison `hook timeout` (`hooks::TIMEOUT_DENIAL`) |
| timeout ailleurs | **fail-open** : hook ignoré, `warn!` structuré, le tour part |
| spawn en échec | même règle que le timeout |

Défaut de timeout : 10 s pour un hook de config (`config::DEFAULT_TIMEOUT_S`), 30 s pour un
hook de plugin (`DEFAULT_HOOK_TIMEOUT_SECS`, inchangé pour ne pas casser l'existant).

## Replay

- Kind `hook_output`, adressé par `(turn_seq, event, addr)` :
  `addr` = id d'appel d'outil pour `pre_tool_use`/`post_tool_use`, vide pour les événements
  de tour. Jamais d'adressage positionnel (règle S3 event log v2).
- Payload : `{turn_seq, event, addr, stdout?, reason?, plugin?}`. `stdout` pour une injection,
  `reason`+`plugin` pour un refus. Écriture seulement quand il y a quelque chose à servir :
  un journal sans ligne pour un appel signifie « rien injecté / appel autorisé ».
- **Gate unique** : `HookManager::with_replay(source)`, posé par `Agent::set_replay_source`.
  Sa présence court-circuite `emit`, `emit_blocking` et `emit_capturing` — les trois seules
  portes vers un `spawn`. Aucun site de branchement n'a de gate `is_replay` à lui : impossible
  d'en oublier un. `Agent::set_hook_manager` reporte le journal sur tout manager remplacé
  après coup, pour que rebrancher des hooks en rejeu ne rouvre pas la porte.
- Les gardes `has_hooks(PreToolUse)` ont été retirés des deux sites de dispatch : en rejeu sur
  une machine sans les hooks, le garde aurait court-circuité le service du refus journalisé.
- `hook_output` ajouté à `PURGEABLE_KINDS` (contenu potentiellement volumineux).

## Décisions de sécurité

1. **Hooks projet inactifs par défaut.** `.kaji/hooks.yaml` vit dans le dépôt : un `git clone`
   suffirait sinon à faire exécuter du shell au premier `kaji` lancé dedans.
   `KAJI_PROJECT_HOOKS=1` (env ou clé de config) est le consentement explicite. Sans lui, le
   fichier est vu, tracé en `debug!`, et ignoré. Testé (`project_hooks_stay_inert_until_the_user_opts_in`).
2. **Hooks user : même modèle de confiance que le reste de la config.** La clé `hooks` de
   `config.yaml` est écrite par l'utilisateur ; pas d'escalade nouvelle.
3. **Payload borné** (cf. ci-dessus) : la surface d'exfiltration se limite au prompt courant,
   au nom/arguments de l'outil et au répertoire de travail.
4. **Fail-closed sur le seul garde-fou.** Un `pre_tool_use` muet (timeout, spawn cassé) bloque :
   un garde-fou qui n'a pas pu répondre n'est pas un garde-fou qui a dit oui. Partout ailleurs
   le fail-open protège la disponibilité du tour.
5. **Config cassée ≠ démarrage cassé.** Clé `hooks` illisible, YAML mal formé, événement
   inconnu, matcher regex invalide : `warn!` puis ignoré.

## Tests

| Suite | Comptes |
|---|---|
| `crates/kaji/tests/hooks_lifecycle_test.rs` (nouveau) | 6 tests, chacun joué sur les **deux** boucles (`KAJI_STATE_MACHINE` absent / `1`) = 12 exécutions |
| `hooks/config.rs` (unitaires, nouveaux) | 4 |
| `replay/source.rs` (unitaire, nouveau) | 1 (`hook_output_and_denial_are_served_for_the_replayed_turn`) |
| `cargo test -p kaji --lib` | **1985 passés, 0 échec** |
| Suites replay (11 fichiers) | toutes vertes |
| `cargo clippy -p kaji --all-targets -- -D warnings` | propre |

Les 6 tests d'acceptation :

1. `session_start_injects_context_into_the_assembled_prompt`
2. `user_prompt_submit_rewrites_the_prompt_and_the_log_carries_it` (vérifie aussi le `message` du journal)
3. `pre_tool_use_blocks_the_matched_tool_and_lets_the_rest_through` (matcher qui colle → bloqué et non exécuté ; matcher qui ne colle pas → rien lancé, outil exécuté)
4. `post_tool_use_feedback_reaches_the_model_with_the_tool_result`
5. `a_slow_context_hook_fails_open_and_a_slow_guard_fails_closed`
6. `a_recorded_session_replays_on_a_machine_without_the_hooks` (compteur de passages du script inchangé après rejeu, prompt rejoué identique)

Le script de refus sort en **exit 7**, pas 2 : c'est bien « exit ≠ 0 » que le test vérifie.

## Écarts et limites

- **Échec préexistant** : `tests/agent.rs::live_tool_result_projects_user_content_but_persists_canonical_result`
  échoue aussi sur `HEAD` propre (vérifié par `git stash` puis re-run). Non causé par cette tâche,
  non listé dans les échecs tolérés du plan — à signaler.
- **`ops_steer`** garde son `UserPromptSubmit` en fire-and-forget : un message injecté en vol
  n'est pas réécrit par les hooks. Hors périmètre S6 (le steer n'ouvre pas un tour) ; à traiter
  si le besoin apparaît.
- **`session_end`** reste fire-and-forget côté CLI, sans capture ni injection — c'est la fin de
  session, il n'y a plus de prompt où injecter.
- **`turn_end`** est fire-and-forget par conception : sa sortie n'entre nulle part, donc rien à
  journaliser ni à servir.
- Les hooks étendus (`before_shell_execution`, `after_file_edit`, …) héritent de la config user
  et du gate de rejeu, mais gardent leur sémantique fire-and-forget d'origine.

## Exemple de config

`~/.config/kaji/config.yaml` :

```yaml
hooks:
  - event: session_start
    command: ~/dotfiles/claude/hooks/shosoin-context.sh
    timeout_s: 15
  - event: user_prompt_submit
    command: ~/dotfiles/claude/hooks/adhd-contract.sh
  - event: pre_tool_use
    matcher: developer__shell
    command: ~/dotfiles/claude/hooks/rtk-rewrite.sh
```

`<projet>/.kaji/hooks.yaml` (nécessite `KAJI_PROJECT_HOOKS=1`) : même forme, liste nue ou
sous clé `hooks:`.
