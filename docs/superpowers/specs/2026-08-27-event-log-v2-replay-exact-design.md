# Event log v2 — replay exact — Design (P2)

Date : 2026-08-27 · Statut : validé (brainstorm S1-S6, go user) · Prochaine étape : plan d'implémentation
Pre-mortem fondateur : 7 scénarios (hermétisme, P1×P2, matching séquentiel, UUID hors IdGen, tokenizer/compaction, GC vs checkpoints, dérive de version) — leurs mitigations sont intégrées ci-dessous. Rapport : decision log mainteneur, hors repo.

## Contexte et objectif

L'event log v1 (`session_events`, spec `2026-08-12-event-log-design.md`) est un journal d'audit :
il enregistre les événements observables (`turn_start`/`turn_end`, `message`, `usage`,
`approval`, `checkpoint`, …) au point de convergence des deux boucles agent —
l'enveloppe de `Agent::reply()` (`crates/kaji/src/agents/agent.rs:2268-2381`) — mais il ne
capture aucune des **entrées** de la boucle : réponses LLM, résultats d'outils, horloge, ids,
bloc mémoire. Une session ne peut être que relue, jamais **rejouée**.

Objectif v2 : chaque session est **rejouable exactement** — la vraie boucle agent re-tourne,
mais toutes ses entrées non-déterministes sont servies depuis le log. Buts, par priorité
(décision user) : 1. debug post-mortem (rejouer une session qui a mal tourné, sans re-payer
les appels LLM) ; 2. tests déterministes (sessions dorées rejouables sans réseau) ;
3. time-travel/branching (le format le permet, l'UX n'est pas construite) ; 4. audit
(sous-produit du log complet).

## Décisions verrouillées

| Décision | Justification |
|----------|---------------|
| Journal des frontières : la vraie boucle re-tourne, ses entrées non-déterministes sont servies depuis le log | Seule option qui serve debug ET tests déterministes ET branching futur ; un viewer de snapshots ne rejoue rien |
| Extension du log v1, pas de nouveau stockage | `session_events` append-only + hook de parité unique déjà établis et éprouvés |
| Outils rejoués depuis le log, jamais ré-exécutés | Replay exact, offline, sans effets de bord ; la ré-exécution live est un mode futur du branching |
| Enregistrement toujours actif | Le debug post-mortem exige que la session déjà cassée soit enregistrée ; le coût se gère par rétention |
| Adressage par clé (`tool_call_id`, `(turn_seq, call_idx)`, `request_hash`), jamais positionnel | Pre-mortem 5 : le matching séquentiel aveugle transforme toute perte en décalage silencieux — le pire mensonge possible pour un outil de debug |
| Replay strict par défaut, hermétique par construction | Pre-mortems 1-3 : un replay qui « réussit » en divergeant est pire qu'un échec bruyant ; un replay qui écrit pollue la mémoire P1 et le log source |
| Rétention par kind, contrat append-only v1 amendé (supersede documenté) | Pre-mortem 7 : un GC par âge indistinct mange les rows `checkpoint` que `/restore` lit |

## S1 — Modèle de données

Table `session_events` existante (`crates/kaji/src/session/session_manager.rs:1199-1213` :
`id, session_id, turn_seq, ts_ms, kind, payload_json`) étendue par de nouveaux `kind` —
aucun changement des kinds v1, aucun nouveau stockage.

**Nouveaux kinds (payload JSON, clés d'adressage en gras) :**

| kind | Contenu | Émis |
|---|---|---|
| `log_meta` | version kaji, `CURRENT_SCHEMA_VERSION`, graine IdGen de session | 1× en tête de session |
| `llm_request` | **`(turn_seq, call_idx)`**, `request_hash` (SHA-256 du payload provider sérialisé), modèle, provider | à chaque appel LLM, avant l'envoi |
| `llm_response` | **`(turn_seq, call_idx)`**, chunks du stream **ordonnés dans un seul événement** (pas un INSERT par chunk), finish reason | à la fin de chaque appel LLM |
| `tool_result` | **`tool_call_id`**, résultat complet (`ToolResponse` sérialisé) | à la fin de chaque exécution d'outil |
| `memory_block` | bloc mémoire P1 splicé **verbatim** (sortie de `splice_memory_block`) | 1× par tour, avant l'appel LLM |
| `condense_triggered` | tour, raison, comptage qui a déclenché | quand la compaction se déclenche |
| `clock_reads` | lectures d'horloge du tour, liste ordonnée **indexée `(turn_seq, idx)`** | 1× par tour (flush au `turn_end`) |

**Règle d'extension (contrat, à inscrire au plan et dans AGENTS.md)** : toute nouvelle source
d'état externe entrant dans le prompt (hints, extensions, futur splice) exige son kind
d'événement. La liste des frontières est ouverte ; l'oublier = pre-mortem 3.

**Ids et horloge** : pas d'enregistrement des ids — `IdGen` est **dérivé déterministe**
(graine de session dans `log_meta` + `(turn_seq, compteur)`), donc identique au replay par
construction. L'horloge est enregistrée (`clock_reads`) car ses valeurs entrent dans le prompt
(`prompt_manager.rs:231`) et les métadonnées (`message.rs:989`).

**Dette v1 payée au passage** : contrainte `UNIQUE(session_id, turn_seq)` sur
l'allocation de tour + `next_turn_seq` transactionnel (aujourd'hui `SELECT MAX+1` non
transactionnel, `session_manager.rs:2557-2564` — race documentée dans la spec v1).

## S2 — Enregistrement

**Hook** : l'enveloppe `Agent::reply()` reste le point unique — les deux boucles (legacy,
state machine via `reply_with_state_machine`, aiguillage `agent.rs:2439-2446`) convergent
dans le même `BoxStream<AgentEvent>` ; parité par construction, comme v1. Les captures LLM
se font au site partagé `stream_response_from_provider`
(`crates/kaji/src/agents/reply_parts.rs:426`) ; les captures outils à leur(s) site(s)
d'exécution — le plan d'implémentation identifie le point exact ; si aucun site n'est
partagé entre les deux boucles, la capture remonte dans l'enveloppe (interception du flux
d'événements) pour préserver la parité. Aucune logique dupliquée dans les fichiers de boucle.

**Writer** : batché asynchrone (le coût INSERT-par-chunk v1 est documenté ; v2 multiplie les
volumes), avec **flush + fsync à chaque `turn_end`**. Un crash laisse au pire un tour
incomplet en queue de log — détectable par le pattern existant
`last_turn_is_interrupted` (`session_manager.rs:2612`) et traité proprement au replay (S3).
Jamais de perte silencieuse au milieu d'un tour committé.

**Non-fatal par construction** (règle v1 conservée) : toute erreur d'écriture du log ⇒
`warn!` + session marquée `replayable = false`, le tour continue. Le log ne casse jamais
un tour vivant.

**Neutralisation du non-déterminisme** :
- Trait `Clock` injecté — remplace les lectures `Utc::now()` de la boucle : `Message.created`
  (`kaji-provider-types/src/conversation/message.rs:989`), `ts_ms` des événements,
  `current_date_timestamp` du prompt système (`prompt_manager.rs:231`). En enregistrement il
  lit l'horloge réelle et journalise ; en replay il sert `clock_reads`.
- Trait `IdGen` injecté — remplace les 3 sites `Uuid::new_v4()`
  (`reply_parts.rs:593`, `state_machine/operation.rs:240`, `message.rs:1015`).
- **Garde outillée** (pre-mortem 4) : clippy `disallowed-methods` sur `uuid::Uuid::new_v4`
  et `chrono::Utc::now` dans les crates de la boucle agent, `#[allow]` ciblés dans les
  implémentations de `Clock`/`IdGen` uniquement. L'invariant est porté par le compilateur,
  pas par la convention.

## S3 — Replay

**Surface** : `kaji replay <session-id> [--until <turn>] [--lenient]`. Headless : imprime la
transcription du replay (tours, messages, outils, divergences) et sort avec un code d'erreur
en cas de divergence. Le viewer TUI interactif est un non-goal v2.

**Strict par défaut** (pre-mortems 1, 5) :
- `ReplayProvider` (implémente le trait `Provider`, branché au même site partagé) sert
  `llm_response` par `(turn_seq, call_idx)` **après** vérification du `request_hash` de la
  requête reconstruite contre `llm_request`. Mismatch ⇒ arrêt avec diff lisible.
  `--lenient` continue en signalant chaque divergence.
- L'intercepteur d'outils sert `tool_result` par `tool_call_id`. Clé absente ⇒ arrêt :
  « log tronqué ou divergent au tour N », jamais de valeur voisine.
- Sequence-addressed, pas content-addressed : le prior art `TestProvider`
  (`providers/testprovider.rs:50-118`, hash-keyed, order-independent) reste réservé aux
  scenario tests ; sa migration vers le format v2 est un non-goal.

**Hermétique par construction** (pre-mortems 2, 3, 6) — un `ReplayMode` traverse
l'enveloppe et impose, pour chaque lecture/écriture de la boucle, sa politique :

| Interaction de la boucle | Politique replay |
|---|---|
| Appels LLM (`stream_response_from_provider`) | servis depuis `llm_response` |
| Exécutions d'outils | servies depuis `tool_result` |
| Bloc mémoire P1 (`splice_memory_block`, `kaji.rs`) | servi depuis `memory_block` — le splice réel est bypassé |
| Décision de compaction | suit `condense_triggered` du log — jamais recalculée (l'appel LLM de résumé est servi comme les autres) |
| Horloge / ids | servis par `Clock` replay / `IdGen` dérivé |
| `ingest_turn` + `maybe_spawn_curation` (mémoire P1) | **désactivés** — zéro écriture dans `shared.db`, zéro appel curateur |
| Snapshot checkpoint (`agent.rs:2210-2252`) | désactivé |
| `usage_ledger`, append `session_events` | désactivés — le replay n'écrit **jamais** dans le log qu'il lit |
| Sortie du replay | session dérivée `replay-of-<id>` (ou éphémère) — la session originale n'est jamais modifiée |

**Sessions incomplètes** : un tour sans `turn_end` en queue de log (crash à
l'enregistrement) ⇒ le replay s'arrête à la dernière frontière de tour saine, avec message
explicite.

## S4 — Migration et cohabitation

- **Sessions pré-v2** : détectées par l'absence de `log_meta` — `kaji replay` répond
  « session enregistrée avant le replay v2 », sans erreur brute. Aucune migration rétroactive
  (les entrées manquantes sont irrécupérables par nature).
- **Contrat append-only v1 amendé** (supersede documenté dans cette spec) : la purge par
  rétention (S5) devient la deuxième suppression autorisée, à côté du cascade DELETE de
  session. Toujours aucun UPDATE.
- **Checkpoints** : les rows `checkpoint` et leur lecture par `checkpoint_restore.rs` sont
  intouchés ; le kind est permanent (S5).
- **Approvals** : le hook `log_approval_event` (`agent.rs:1817-1858`) reste ; au replay les
  approbations sont rejouées depuis les rows `approval` existantes (pas de prompt à
  l'utilisateur).

## S5 — Rétention et coût

- **Purge par kind, jamais par âge indistinct** (pre-mortem 7) : seuls les kinds volumineux
  du replay (`llm_request`, `llm_response`, `tool_result`, `memory_block`, `clock_reads`)
  sont purgeables. Les kinds v1 (`turn_start/end`, `message`, `usage`, `approval`,
  `checkpoint`, `log_meta`) sont **permanents**.
- Config `replay_retention_days` (défaut **30**) ; purge au boot (même timing que les
  migrations de schéma). Session purgée ⇒ `replayable = false` ; `kaji replay` explique
  (« payloads purgés le … , rétention 30 j »).
- Le flag `replayable` vit sur la row `sessions` (colonne, migration additive v17).

## S6 — Erreurs et tests

**Robustesse** : writer non-fatal (S2) ; replay strict fail-fast (S3) ; hermétisme garanti
par le mode, pas par la discipline (S3).

**Plan de test (obligatoire au plan d'implémentation)** :
1. **Doré** : session synthétique enregistrée (provider fixture) → `kaji replay` ×2 → les
   deux transcriptions sont byte-identiques entre elles et cohérentes avec le log.
2. **Hermétisme** : replay complet ⇒ zéro delta sur `shared.db`, `.kaji/memory/`,
   le log source et `usage_ledger` (comparaison avant/après).
3. **Troncature** : log amputé au milieu du dernier tour ⇒ replay s'arrête à la frontière
   précédente, message explicite, code de sortie dédié.
4. **Parité** : même session enregistrée sous la boucle legacy et sous
   `KAJI_STATE_MACHINE=1` ⇒ même séquence de kinds, mêmes clés (les tests de parité v1
   existants s'étendent aux nouveaux kinds).
5. **Strict-mismatch** : replay après modification artificielle du prompt système ⇒ arrêt au
   tour 1 avec diff ; `--lenient` continue et compte les divergences.
6. **Non-fatal** : erreur SQLite injectée pendant l'enregistrement ⇒ le tour aboutit,
   session marquée `replayable = false`.
7. **Clippy gate** : `disallowed-methods` actif — un `Uuid::new_v4()`/`Utc::now()` nu dans
   les crates de la boucle fait échouer le lint.

## Non-goals

- Viewer TUI interactif du replay (le runner headless suffit au but 1 en v2).
- Branching / time-travel UX — le format le permet (préfixe rejoué puis intercepteurs
  débranchés), rien n'est construit.
- Migration de `scenario_tests`/`TestProvider` vers le format v2.
- Ré-exécution live des outils au replay.
- Compression des payloads ; sync réseau.
