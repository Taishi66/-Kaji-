# Volet 炉 forge — « qui fait quoi » des subagents dans la TUI — Design

> Contrôleur : Fable (brainstorm, spec, rulings). Décisions user en session 2026-08-20 : **volet latéral** (contre lignes de chat, mission control ou hybride), **Enter = fiche détail** (contre flux live), **auto-forge** (ouverture/repli automatiques + toggle manuel), **double alimentation** (snapshot + notifications, contre notifications seules ou canal dédié).

## But

Rendre visible et pilotable, en temps réel, tout ce qui travaille en parallèle dans une session kaji : l'agent principal et chaque subagent (`delegate`/`load`, extension summon) — statut, outil en cours, durée — dans un volet latéral qui s'allume quand la forge travaille et s'éteint quand elle a fini.

## Constat (vérifié au code, 2026-08-20)

- Chaque tool call d'un subagent émet déjà une `ServerNotification` typée `subagent_tool_request` avec `subagent_id` et `tool_call.name` (`crates/kaji/src/agents/subagent_handler.rs:286-308`).
- Les deux boucles d'agent la relaient en `AgentEvent::McpNotification` : legacy `agent.rs:3206,3223`, state machine `ops_toolcalling.rs:813,827` — **parité déjà assurée, aucun changement de boucle requis**.
- La TUI la jette : `apply_agent_event`, bras `_ => {}` (`crates/kaji-cli/src/tui/app.rs:3157`).
- **Hors tour** (subagent async, parent idle), les notifications buffrent dans summon (`summon.rs:533-545`) : le flux d'événements seul ne suffit pas — d'où la double alimentation.
- Identité disponible côté summon : `session.id` du subagent, `parent_session_id` persisté (`session_manager.rs:94`), description de tâche, compteur de tours, timestamp d'activité, `CancellationToken` par tâche.
- `TaskExecutionNotificationEvent`/`TaskInfo` (`subagent_execution_tool/notification_events.rs`) existent sans producteur actif — **non utilisés par ce design** (le snapshot interroge le registre summon directement ; pas de nouveau protocole de notification à inventer).

## Architecture

Trois étages, du core vers l'écran :

1. **Core (crates/kaji)** — deux méthodes nouvelles sur `Agent` :
   - `subagent_snapshot() -> Vec<SubagentTaskSnapshot>` : lit le registre summon (`background_tasks` + `completed_tasks`) et rend `{ subagent_id, description, status, turns, started, last_activity, finished, result_summary: Option<String>, error: Option<String> }`. Vide si l'extension summon n'est pas chargée.
   - `cancel_subagent(subagent_id) -> bool` : déclenche le `CancellationToken` de la tâche ; `false` si inconnue ou déjà terminée.
2. **État (TUI)** — nouveau module `crates/kaji-cli/src/tui/forge.rs` :
   - `ForgeTask { subagent_id, description, status: ForgeStatus (EnCours | Terminé | Échec | Annulé), current_tool: Option<String>, started, finished, turns }`.
   - Dans `App` : `forge_tasks: BTreeMap<String, ForgeTask>` (ordre stable par id), `forge_selected: usize`, `forge_view: ForgeView (Auto | ForcéOuvert | ForcéFermé)`, `forge_folds_at: Option<Instant>`.
   - Réconciliation : les deux sources écrivent dans `forge_tasks` par clé `subagent_id` — le snapshot fait autorité sur le statut, la notification sur `current_tool`. Une tâche absente d'un snapshot alors qu'elle était `EnCours` passe `Terminé` (jamais de ligne qui s'évapore sans état final).
3. **Rendu (TUI)** — volet colonne droite dans `ui.rs`, gabarit et marges de l'explorateur (T1 2026-08-19), largeur ~32 colonnes.

## Alimentation

- **Snapshot** : tick forge 1 s dans `mod.rs` (même patron que le tick 250 ms : armé seulement si `!forge_tasks.is_empty() || forge visible`), qui appelle `subagent_snapshot()` et réconcilie. C'est lui qui fait vivre le volet quand le parent est idle.
- **Live** : `apply_agent_event` gagne un bras `AgentEvent::McpNotification` qui ne traite que les `LoggingMessageNotification` dont le data JSON a `type == "subagent_tool_request"` : extrait `subagent_id` et `tool_call.name`, met à jour `current_tool` (crée l'entrée si le snapshot ne l'a pas encore vue). Tout autre payload : ignoré sans bruit. Payload malformé : ignoré sans panic.

## Comportement du volet (auto-forge)

- Apparaît seul quand `forge_tasks` passe de vide à non-vide (`forge_view == Auto`).
- Quand la dernière tâche `EnCours` se termine : `forge_folds_at = now + 5 s`, puis repli automatique (les terminées restent listées tant que le volet est ouvert). Une nouvelle tâche annule le repli.
- `Ctrl+F` (vérifié libre) et `/forge` : si le volet est visible → `ForcéFermé` ; sinon → `ForcéOuvert`. `Forcé*` gagne sur l'automatique ; l'apparition d'une **nouvelle** tâche ramène `forge_view` à `Auto` (le volet réapparaît).
- `Ctrl+O` intègre le volet au cycle de focus existant (composer → explorateur → lecteur → forge).
- Barre d'état : `遣 N` (muted) quand N tâches tournent et que le volet est fermé — l'activité n'est jamais invisible.

## Contenu et interactions

- **Ligne 0** : agent principal — sceau du mode (statusbar existante), `火 {current_tool()}` pendant un tour, `思` sinon. Non sélectionnable (`↑/↓`, `Enter` et `x` ne s'appliquent qu'aux subagents).
- **Une entrée par subagent** : `{glyphe statut} 遣 {description tronquée}` + seconde ligne `火 {outil} · {durée}` ou `{statut} · {durée}`. Glyphes : `◐` en cours (spinner `blade_frame`), `✓` terminé, `✗` échec/annulé — couleurs palette `theme.rs` uniquement (accent actif, muted terminé, accent échec).
- `↑/↓` (et `j/k`) : sélection. `Enter` : fiche détail dans le **lecteur existant** — description complète, statut, durée, tours, dernier outil, résultat (ou erreur) si finie ; rafraîchie à chaque tick/notification tant qu'elle est ouverte.
- `x` sur une tâche `EnCours` : confirmation y/n (même patron que `/restore`), puis `cancel_subagent(id)` ; la ligne passe `✗ annulé`.

## Bords et erreurs

- Pas d'extension summon : volet jamais visible en Auto ; `Ctrl+F` pousse la ligne système `forge : aucune tâche`.
- Terminal étroit (< ~90 colonnes) : même arbitrage que l'explorateur aujourd'hui (le volet prend le pas sur le chat).
- Hauteur 12 lignes : aucun panic, `saturating_sub` partout (invariants T1).
- `cancel_subagent` sur une tâche déjà finie : no-op, ligne système `forge : tâche déjà terminée`.

## Tests

- `forge.rs` : réconciliation (snapshot puis notification, notification avant snapshot, disparition du snapshot → `Terminé`), machine Auto/Forcé/repli 5 s, tri stable.
- `app.rs` : bras `McpNotification` (payload valide, autre type, malformé), `遣 N` dans la barre.
- `ui.rs` : volet présent/absent, sélection, fiche lecteur, largeur 60 et hauteur 12 sans panic.
- Core : `subagent_snapshot()` sans summon → vide ; avec tâches (réel ou fake registre) → champs remplis ; `cancel_subagent` inconnu → `false`.

## Hors périmètre (v1)

- Flux live du transcript d'un subagent dans le lecteur (v2 possible — le canal `notification_subscribers` de summon existe).
- Arbre multi-niveaux (subagents de subagents) : la v1 liste à plat.
- Sessions ACP multiples (`acp/server.rs:202`) : autre sujet, autre surface.
- Producteur `TaskExecutionNotificationEvent` : inutile ici.
