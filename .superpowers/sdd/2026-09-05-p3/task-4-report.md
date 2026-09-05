# Task 4 — Workflows : exécuteur DAG + events v2

Plan : `docs/superpowers/plans/2026-09-05-p3-vision-web-workflows-mission-control.md`
Spec (autorité) : `docs/superpowers/specs/2026-09-05-p3-vision-web-workflows-mission-control-design.md`, § S3.

## Architecture

`crates/kaji/src/workflow/` — 6 modules + un module de tests.

| Module | Rôle |
|---|---|
| `state.rs` | `AgentState` / `StageState` (`Pending/Running/Waiting/Done/Failed(cause)/Cancelled`), `FailureCause::{Budget(BudgetLimit), Error}`, `WorkflowState` (snapshot complet stage → agents) |
| `artifacts.rs` | Table `(stage, agent) → sortie` et substitution `{{stage.agent.output}}` |
| `gate.rs` | `GateDecision`, trait `GateSource`, `LiveGates` (attente d'une décision humaine), `ReplayGates` (décisions du journal) |
| `events.rs` | Les 7 kinds v2, leurs payloads typés, et `WorkflowRecorder` (capture) |
| `runner.rs` | Trait `AgentRunner` (seam de spawn) + `SubagentRunner` (production) |
| `executor.rs` | `WorkflowExecutor` (ordonnanceur DAG) et `WorkflowHandle` (contrôle live) |

### Ordonnancement

Boucle unique sur un `FuturesUnordered` de futures de stage : à chaque tour, tout stage dont **toutes** les dépendances sont `Done` part ; deux stages sans lien partent donc ensemble. L'ordre est **topologique**, jamais l'ordre du document — les tests exercent explicitement un `synthese` déclaré avant sa dépendance `collecte` (note T3 : `depends_on` vers un stage postérieur en liste est légal).

Le fan-out d'un stage est un `join_all` sur ses agents. La concurrence est coopérative (un seul task pilote tout) : les agents sont IO-bound (attente d'une session enfant), aucun n'occupe le thread.

Un stage qui n'aboutit pas — gate refusée, agent en échec, annulation — passe la fermeture transitive de ses descendants en `Cancelled` (calculée sur les stages encore en attente uniquement).

### Gates

`gate: approve` fait passer le stage en `Waiting` **avant** d'exécuter ses agents, puis attend `GateSource::decide`. `WorkflowHandle::{approve,deny}(stage)` alimente `LiveGates` (compteur `watch` comme réveil : un waiter s'abonne avant de lire la table, donc pas de réveil perdu). `deny` → stage `Cancelled` + descendants `Cancelled`, aucun agent lancé.

`WorkflowHandle::approve/deny` rend `false` quand le workflow ne prend pas de décision vivante (cas rejeu) : T5/T6 peuvent le remonter en message plutôt qu'en attente silencieuse.

### Budgets

- `max_duration_s` : `select!` contre un `sleep` — dépassement → `CancellationToken` tiré (le jeton existant, celui que summon passe déjà à `run_subagent_task`), `CANCEL_GRACE` de 5 s pour un arrêt propre, état `Failed(Budget(Duration))`.
- `max_tokens` : garde qui relit `AgentRunner::tokens_used(session_id)` toutes les 500 ms (production : `get_session_usage_totals` de la session **enfant**). Le dépassement se constate à un tick près — un budget de tokens borne le coût, il ne coupe pas au token exact ; c'est documenté dans le code.

Un budget absent ne participe pas à la course (`std::future::pending`).

## Kinds v2 et payloads

Écrits sur la session **parente**, sous un `turn_seq` unique réservé après `append_log_meta_if_absent` (donc le tour 2 d'une session neuve). Aucun `turn_start`/`turn_end` : ce n'est pas un tour d'agent, et une borne ouverte ferait refuser `EventCursor::load`.

| Kind | Payload | Rétention |
|---|---|---|
| `workflow_started` | `{workflow, spec}` (la `WorkflowSpec` sérialisée entière) | permanent |
| `stage_started` | `{stage, gate}` | permanent |
| `agent_started` | `{stage, agent, session_id, model}` | permanent |
| `gate_decision` | `{stage, decision}` | permanent — **servi au rejeu** |
| `workflow_artifact` | `{stage, agent, output}` | **purgeable** |
| `agent_done` | `{stage, agent, session_id, state, tokens, duration_ms}` | permanent |
| `workflow_done` | `{workflow, state}` (le `WorkflowState` final complet) | permanent |

Tous portent `turn_seq` dans le payload, comme les kinds de la vague C/D.

### Décision — permanent vs purgeable

La sortie d'un agent est le **seul** payload volumineux de la famille. Elle a donc été **tranchée** de `agent_done` (qui n'en garde que les compteurs) vers un kind à part, `workflow_artifact`, ajouté à `replay::retention::PURGEABLE_KINDS` (10 → 11). Les six autres kinds sont structurels et petits : ils sont l'historique du workflow au même titre que `turn_start`, et restent permanents. Conséquence assumée : une session dont le journal a passé la fenêtre de rétention garde sa topologie et ses décisions, perd les sorties d'agents (et est de toute façon marquée `replayable = 0`).

### Capture + service (règle AGENTS.md)

Une seule de ces entrées est **servie** au rejeu : `gate_decision`. C'est la seule qui pilote le déroulé — une approbation humaine est de l'état externe, et un rejeu qui la redemanderait ne serait plus hermétique. Les autres décrivent ce qui s'est passé.

`EventCursor` gagne deux index (patron exact de la vague C/D — adressage par clé, jamais positionnel) :
- `gate_decisions: HashMap<String, GateDecision>` — clé = nom de stage. **Invariant v1** : un stage n'ouvre sa gate qu'une fois par exécution, et une session parente ne porte qu'un workflow (`kaji workflow run` crée sa session). Si T5 permet plusieurs runs par session, la clé devra devenir `(turn_seq, stage)`.
- `workflow_artifacts: HashMap<(String, String), String>` — clé = `(stage, agent)`.

`ReplayGates::from_cursor` sert les décisions ; en strict une gate absente **arrête le stage** (erreur nommée), en lenient elle est refusée avec un `warn!` — un rejeu ne peut pas inventer une approbation.

### Substitution d'artefacts

`Artifacts::substitute` réutilise `kaji_core::workflow::spec::input_references` — la **même** fonction que la validation de la spec — et réécrit les spans de la fin vers le début (les spans des occurrences précédentes restent valides quand la sortie n'a pas la longueur du gabarit). Une référence sans artefact reste écrite telle quelle : la validation garantit que l'ancêtre a tourné, donc un trou est un bug à voir, pas une chaîne vide à avaler.

Seules les **entrées** (`agent.inputs`) sont substituées, jamais le prompt : c'est ce que la spec T3 valide (`validate_input_references` n'itère que `agent.inputs`). Substituer dans le prompt laisserait passer des références non validées.

### Spawn — réutilisation de summon

`SubagentRunner` descend sur `agents::subagent_handler::run_subagent_task` : le **même** primitif que l'outil `delegate` de summon. Le workflow n'ouvre pas un second mécanisme — il assemble la recette (prompt libre + bloc `# Inputs`, ou recette locale + paramètres) et la `TaskConfig` comme summon, puis passe la main au chemin partagé. `parent_session_id` est posé par `prepare`, qui crée la session enfant en `SessionType::SubAgent` et rend son id **avant** l'exécution — d'où un `agent_started` qui porte déjà l'id de la session enfant, et un garde de tokens qui peut lire l'usage pendant que ça tourne.

`kaji-core` : `Deserialize` ajouté sur `WorkflowSpec`, `Stage`, `AgentSpec`, `AgentSource` (round-trip du payload `workflow_started`, `AgentSource` externally-tagged snake_case). Le module-doc précise que ce chemin court-circuite la validation et ne doit jamais être branché sur de l'entrée utilisateur.

## Parité legacy / machine à états

L'exécuteur vit **hors des deux boucles** : il pilote des sessions, il n'en est pas une. Ses deux seuls points de contact avec la boucle sont des sites déjà partagés — `SessionManager::append_event` pour le journal, `run_subagent_task` pour le spawn. **Rien à appliquer deux fois** : aucun fichier de `agents/agent.rs` ni de `agents/state_machine/` n'est touché. La seule modification hors `workflow/` et `replay/` est l'ajout des deux champs de `EventCursor` aux 4 constructeurs littéraux existants (tous des helpers de test).

## Tests

**15 nouveaux, tous verts.** 13 en `--lib workflow`, 2 en intégration.

`crates/kaji/src/workflow/` (13) :
- `artifacts` ×2 — substitution multi-occurrences ; référence sans artefact laissée visible
- `gate` ×2 — réveil d'un waiter enregistré avant la décision ; réponse du journal sans décision vivante + refus strict d'une gate absente
- `tests.rs` ×9 — DAG fan-out ×2 + dépendant (ordre topologique malgré l'ordre du document, substitution des deux artefacts) ; gate qui bloque puis approuve ; gate refusée → stage + descendants `Cancelled`, zéro agent lancé ; budget durée coupe et nomme le budget ; budget tokens coupe sur l'usage de la session enfant ; agent en échec → stage `Failed` + descendants `Cancelled` ; **les 7 kinds journalisés dans l'ordre exact** ; gate servie du log au rejeu ; rejeu strict sans gate enregistrée → stage `Failed`, pas d'attente

`crates/kaji/tests/workflow_dag_test.rs` (2) :
- workflow enregistré → rejoué avec `ReplayGates::from_cursor`, mêmes agents dans le même ordre, mêmes entrées substituées, état final **identique**, et `handle.approve()` refusé pendant le rejeu
- `SubagentRunner::prepare` rattache la session enfant au parent (`parent_session_id`, `SessionType::SubAgent`, nom `stage.agent`)

Suites complètes : `cargo test -p kaji --lib` → **1991 passed, 0 failed**. `cargo test -p kaji-core` → vert. `replay_retention_test` / `replay_schema_test` / `replay_record_test` / `replay_intercept_test` → verts. `cargo clippy -p kaji -p kaji-core --all-targets -- -D warnings` → **zéro warning**.

## Écarts

1. **Fixture au niveau `AgentRunner`, pas au niveau provider.** Les tests remplacent le lancement d'un sous-agent (le seul point qui exigerait un provider + une boucle agent complète) et exercent le code de production pour tout le reste — ordonnancement, artefacts, gates, budgets, journal, `EventCursor`. Le journal écrit est le vrai journal v2 d'un vrai `SessionManager`. Le bout-en-bout provider→boucle→journal est le doré T8.
2. **Pas de kind `stage_cancelled`.** Un stage annulé en cascade n'émet aucun event : la cascade est expliquée par le `gate_decision`/`agent_done` qui l'a déclenchée, et l'état final complet vit dans `workflow_done`. Si T6 veut la cascade en direct, elle vient de `WorkflowHandle::snapshot()`.
3. **`tests.rs` dans le lib plutôt que dans `tests/`** (AGENTS.md § Rules préfère `tests/`) : le harness a besoin du fixture runner et de `WorkflowRecorder` en interne, et la commande d'itération du plan est `cargo test -p kaji --lib workflow`. Le contrat public est couvert par `tests/workflow_dag_test.rs`.
4. **Budget tokens à 500 ms près** (voir supra). Aucun mécanisme d'interruption au token exact n'existe dans la boucle enfant.
5. `SubagentRunner::run` n'a pas de test bout-en-bout (il faudrait un provider). `prepare` l'est.

## Consignes pour T5 (CLI)

- Point d'entrée : `WorkflowExecutor::new(spec, Arc::new(SubagentRunner::new(session_manager)), recorder, parent_session_id, working_dir)`, `handle()` **avant** `run()` (qui consomme `self`), puis `tokio::spawn(executor.run())`.
- `WorkflowRecorder::open(session_manager, session_id)` pose `log_meta` et réserve le `turn_seq` : à appeler une fois par run.
- La session parente doit porter un `provider_name` (et idéalement un `model_config`), sinon `SubagentRunner::run` échoue avec « aucun provider configuré sur la session parente ». À poser à la création de la session workflow.
- Code de sortie : `WorkflowState::outcome()` rend `Done` / `Failed(cause)` / `Cancelled`.
- `list`/`status` : tout est relisible depuis le journal — `workflow_started` porte la spec, `workflow_done` l'état final. Une session workflow se reconnaît à la présence d'un `workflow_started`.
- Contrôle live déjà exposé : `approve`, `deny`, `cancel`, `snapshot`. **Manquent** (non demandés par S3 côté T4) : `pause/reprise` de stage et `steer` — à ajouter sur `WorkflowHandle` + `Shared` en T5 ; le patron `LiveGates` (table + `watch`) se réplique tel quel pour la pause.

## Consignes pour T6 (mission-control)

- `WorkflowHandle::snapshot() -> WorkflowState` est la source de vérité live : `stages[].{name, state, gate, agents[]}`, chaque `AgentStatus` portant `{name, state, session_id, tokens, duration_ms}`. `state.label()` rend déjà un libellé court FR pour la carte.
- Colonnes = `WorkflowState.stages` dans l'ordre de la spec ; le `session_id` de chaque agent joint `subagent_snapshot` et l'usage ledger.
- `StageState::Waiting` = gate en attente → c'est la touche `g` du plan T7 ; `handle.approve(stage)` rend `false` si le workflow est un rejeu (afficher « rejeu : gate servie du journal », ne pas attendre).
- `FailureCause::Budget(limit)` donne `limit.field()` (`max_tokens` / `max_duration_s`) pour nommer le budget dépassé dans la carte.
