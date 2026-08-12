# Spec — Event log append-only par session (kaji)

Statut : approuvée en design (2 forks tranchés en session 2026-08-12 : stockage = table dans
sessions.db ; portée MVP = audit + détection de tour interrompu). Inspiration : Muse Code
(runtime replay-exact, restart-safe) + Zed DeltaDB (traçabilité prompt→issue).
Scope : crates/kaji (flux d'événements + persistance) + kaji-cli (détection au resume).
Aucun changement de comportement agent-loop — seulement de l'observation append-only.

## Objectif

Journaliser chaque tour au fil de l'eau, de façon append-only, pour :
1. **Restart-safe** — un crash/kill en plein tour ne perd plus le tour (aujourd'hui la table
   `messages` ne stocke que les messages *terminés*, via `add_message`).
2. **Approbations d'outils y/n** — aujourd'hui **jamais persistées** : la décision transite par
   un `oneshot` en mémoire (`ToolConfirmationRouter`, tool_confirmation_router.rs:8-46) et
   disparaît. Le log les capture → audit « qu'ai-je autorisé, quand ».
3. **Trace d'audit** par tour : prompt → événements → issue, relisable hors-ligne.

## Non-objectifs (YAGNI, v2+ par-dessus ce socle)

- Replay-exact reconstructif d'un tour (re-streaming des deltas, re-exécution). Le MVP journalise
  et *détecte* un tour interrompu ; il ne le *rejoue* pas.
- Checkpoints git / undo d'arbre de travail (backlog « composant checkpoints » — même substrat
  visé plus tard, mais l'event log est autonome et utile seul).
- Purge/rotation/rétention du log (v2 : le log grandit avec la session, borné par la vie de la
  session ; cascade DELETE à la suppression de session suffit au MVP).

## Décisions tranchées

- **Stockage** : nouvelle table `session_events` dans `sessions.db` (schéma actuel v15 →
  **v16**), pas de 3e fichier. Réutilise le pool sqlx paresseux, WAL, `busy_timeout(30s)`,
  la clé `session_id`. Motif : `shared.db` (mémoire) rouvre une connexion + scan FS par appel
  (kaji.rs:57-67) — inadapté à un flux d'append ; `sessions.db` a déjà le seul précédent
  append-only pur du code (`usage_ledger`, session_manager.rs:1105-1119).
- **Point d'insertion** : `Agent::reply()` (agent.rs:1811-1824) — **seul** point où les deux
  chemins (legacy `reply_impl` et state-machine via `Emitter`) convergent avant de sortir vers
  les 7 consommateurs. Un hook ici = parité legacy/SM **par construction** (même garantie que
  `condense` dans `stream_response_from_provider`). Le combinateur `.map_ok(ensure_message_event_id)`
  déjà présent devient le point d'accroche : conserver l'assignation d'ID **et** appender au log.

## Schéma `session_events`

```sql
CREATE TABLE IF NOT EXISTS session_events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id  TEXT    NOT NULL,
    turn_seq    INTEGER NOT NULL,   -- n° de tour dans la session (1 par appel reply())
    ts_ms       INTEGER NOT NULL,   -- horodatage ms, même base que usage_ledger
    kind        TEXT    NOT NULL,   -- voir enum ci-dessous
    payload_json TEXT   NOT NULL,   -- variant sérialisé (peut être "{}" pour les marqueurs)
    FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_session_events_session ON session_events(session_id, turn_seq, id);
```

Append-only : **INSERT uniquement**. Le seul DELETE autorisé est la cascade de `delete_session`
(session supprimée = audit supprimé, cohérent). `truncate_conversation` / `HistoryReplaced`
(compaction) ne touchent PAS `session_events` — le log garde la trace même de ce que la
conversation a effacé (c'est une valeur, pas un bug).

### `kind`

- `turn_start` — émis à l'appel de `reply()`, payload = `{ query_preview, kaji_mode }` (borné,
  ex. 200 car.). Ouvre le tour `turn_seq`.
- `turn_end` — émis quand le stream du tour se termine **normalement** (chaîné en fin de stream).
  Un crash/kill/annulation Esc = pas de `turn_end` → **c'est le signal d'un tour interrompu**.
- `message` — un `AgentEvent::Message` (payload = le `Message` sérialisé, déjà JSON-able comme
  dans `messages.content_json`).
- `usage`, `message_usage`, `mcp_notification`, `history_replaced` — les autres variants
  `AgentEvent` (agent.rs:277-286), payload = variant sérialisé.
- `approval` — décision y/n, capturée par un **2e hook** dans `Agent::handle_confirmation`
  (agent.rs:1553-1575), le seul endroit où la permission est connue (jamais un `AgentEvent`).
  payload = `{ request_id, tool_name?, permission: "AllowOnce|DenyOnce|..." }`.

## Détection de tour interrompu (au resume)

Au `--resume`/`seed_chat` (tui/mod.rs:552-557), après avoir rejoué les `messages` :
1. Lire le dernier `turn_seq` de `session_events` pour la session.
2. Si ce tour a un `turn_start` **sans** `turn_end` correspondant → tour interrompu.
3. Afficher une ligne système : `⚠ tour interrompu au resume — N événements journalisés non
   terminés` (+ éventuellement le `query_preview` du `turn_start`). Purement informatif au MVP
   (pas de reprise auto).

## Question d'implémentation ouverte (à résoudre au plan)

`Agent` (crates/kaji) a-t-il déjà un accès à la persistance (`SessionManager`/`SessionStorage`)
au point `reply()` ? Si oui → appender directement. Sinon → deux options à départager au plan :
(a) threader un sink d'écriture léger (trait `EventSink` injecté à la construction de l'Agent,
implémenté par kaji-cli sur SessionManager) ; (b) émettre les événements et laisser le
consommateur unique les persister — **rejeté a priori** car il y a 7 consommateurs (perte de
parité). L'option (a) préserve le point unique.

## Limitations MVP connues (review 18 agents 2026-08-12 — à traiter en v2)

- **Reentrance élicitation** : une réponse d'élicitation ré-appelle `reply()` pendant qu'un tour est ouvert → un 2e `turn_start` imbriqué + approbations attribuées au nouveau `turn_seq`. N'affecte que la *précision de l'audit*, pas la conversation. v2 : compteur de profondeur, journaliser au tour le plus externe.
- **Pas de contrainte `UNIQUE(session_id, turn_seq)`** : `next_turn_seq` = `SELECT MAX+1` non transactionné. Invariant applicatif « un Agent = une session à la fois » (documenté agent.rs) ; deux process kaji reprenant la même session en parallèle pourraient courir la race (impact borné au bandeau « tour interrompu », pas de corruption d'historique).
- **INSERT par chunk non batché** : un tour à N deltas = N INSERT awaités dans le hot-path (WAL, ~sub-ms chacun ; négligeable par token, mais non borné). v2 : batch par tour ou writer async découplé.
- **`handle_confirmation` attend l'écriture DB** de l'approbation avant de livrer la permission — latence sous contention d'écriture sqlite. v2 : append fire-and-forget ou après livraison.
- **Annulation Esc** = fin propre du tour (le loop interne break, `turn_end` écrit) → **non** classée « interrompu » (comportement voulu : l'utilisateur sait qu'il a annulé ; « interrompu » vise crash/kill). Écart assumé vs la formulation initiale du § kind.

## Vérification

- Tests kaji : (1) `session_events` créée à la migration v16, idempotente ; (2) un tour complet
  journalise `turn_start … message* … turn_end` dans l'ordre, `turn_seq` incrémenté au tour
  suivant ; (3) une approbation y puis n journalise 2 `approval` avec la bonne permission ;
  (4) un stream droppé avant épuisement (simulé) ne journalise **pas** `turn_end` ;
  (5) parité : le même scénario sous `KAJI_STATE_MACHINE=1` et sans produit la même séquence
  d'événements (verrou de parité, comme les tests condense).
- Tests kaji-cli : détection au resume — session avec `turn_start` sans `turn_end` → ligne
  système « tour interrompu » ; session propre → aucune ligne.
- Baseline verte (kaji-cli 454, kaji --lib 8 échecs préexistants inchangés) ; clippy scoped ;
  fmt ; migration testée sur une DB v15 existante (pas de perte).
- E2E tmux : lancer un tour, kill -9 en plein stream, `kaji --resume` → la ligne « tour
  interrompu » apparaît.
