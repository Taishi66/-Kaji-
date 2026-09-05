# P3 — Vision, Web natif, Workflows, Mission-control (design)

Décisions user 2026-09-05 (4 réponses au brainstorm) : vision = **attacher + provider** (pas de rendu inline terminal en v1) ; web = **provider-native + fallback configurable** ; multi-agents = **workflow replay + contrôle live** comme différenciateur ; panneau = **TUI mission-control** (le web réutilisera les mêmes events plus tard).

Objectif produit : combler les 3 gaps majeurs face à Claude Code/Codex (vision, web natif, orchestration) en capitalisant sur ce que kaji est seul à avoir — **l'event log v2 / replay exact** et l'esthétique hanko du TUI.

## S1 — Vision : images attachées au message

- `@chemin/image.png` dans le composer (et le fuzzy finder `Tab`) attache l'image au message sortant : `MessageContent` image (base64 + mime), ligne placeholder dans le chat `画 capture.png (1.2 Mo)`.
- Formats v1 : png, jpg/jpeg, webp, gif (non animé). Cap taille : 5 Mo par image, 3 images par message — au-delà, ligne d'erreur et l'image n'est pas attachée.
- Extension de `mentions::expand_mentions` : une mention dont l'extension est image → attachement au lieu d'inclusion texte. Le lecteur (`viewer`) affiche `[binaire image — e pour ouvrir dans l'éditeur]` comme aujourd'hui.
- Provider sans vision → erreur provider propre (kind existant), pas de dégradation silencieuse.
- **Replay** : l'image entre dans `llm_request` (haché) et dans le payload `message` du log — rejouée telle quelle. Coût de log assumé (kinds purgeables, rétention T12). Parité : chemin de message partagé, rien à dupliquer.

## S2 — Web natif : extension platform `web`

- Nouvelle extension platform `web` (patron `tom`/`skills`) : outils `web_search {query, count≤10}` et `web_fetch {url, mode: markdown|raw}`.
- **Backends de recherche** derrière un trait `SearchBackend` : `brave`, `tavily` (clé API), `searxng` (URL d'instance), sélection `KAJI_WEB_SEARCH_BACKEND` + clé/URL en config ; **provider-native** (server tool Anthropic/OpenAI) est prévu par le trait et branché là où l'intégration est triviale, sinon follow-up documenté — le fallback HTTP est la garantie de service.
- **Fetch** : reqwest, timeout 30 s, cap 2 Mo, redirects ≤ 5 ; `markdown` = extraction lisible (readability + html2text), `raw` = texte brut tronqué.
- **Sécurité (review Opus obligatoire)** : garde SSRF par défaut — résolution DNS puis refus des IP privées/loopback/link-local (et de `file://`, ports exotiques) ; opt-out explicite `KAJI_WEB_ALLOW_PRIVATE=1` pour les usages self-hosted. User-agent identifiable `kaji/<version>`.
- **Replay** : `tool_result` déjà capturé/servi (T5/T10) — le web rejoue hermétiquement sans réseau. C'est l'argument : la recherche d'hier se rejoue à l'identique.

## S3 — Workflows : orchestration déclarative + contrôle live + replay

- **Spec YAML** (`kaji workflow run <fichier>`) : `stages` ordonnés ; chaque stage = `agents` (fan-out d'une liste ou map), `depends_on`, `gate: auto|approve`, budgets (`max_tokens`, `max_duration`) ; chaque agent = `recipe|prompt`, `model`, entrées templatées depuis les sorties des stages précédents.
- **Exécuteur** : ordonnanceur DAG au-dessus de summon (sync/async/cancel existants) ; états `Pending/Running/Waiting(gate)/Done/Failed/Cancelled` ; sorties d'agents = artefacts nommés réinjectables.
- **Contrôle live** (via mission-control) : pause/reprise d'un stage, cancel d'un agent (existant), **steer** = injection d'un message dans un agent en vol, approve/deny des gates.
- **Événements** : kinds v2 `workflow_started/stage_started/agent_started/agent_done/gate_decision/workflow_done` — règle AGENTS.md replay appliquée dès la conception : chaque kind est capturé ET servi.
- **Workflow replay — le différenciateur** : la session parente journalise la topologie + les décisions de gates ; les sessions d'agents ont `parent_session_id` (existant) et leurs propres logs v2. `kaji replay <workflow-session>` rejoue la session parente hermétiquement (gates depuis le log) ; v1 rejoue chaque agent individuellement (`kaji replay <agent-session>`), le rejeu orchestré bout-en-bout de l'arbre = follow-up nommé.
- CLI : `kaji workflow run|list|status`. Budgets dépassés → agent coupé proprement + événement.

## S4 — Mission-control : forge 炉 plein écran

- Le volet forge existant reste (32 col) ; **nouvelle vue plein écran** (touche depuis le volet, ou `/forge full`) : colonnes = stages du workflow (ou « libre » pour les summons hors workflow), cartes agent 3 lignes — nom 遣, statut (lame animée `blade_frame` sur l'horloge de la tâche, ✓/✗/⏸), outil courant, `炭 in↑ out↓ · $coût · durée` live depuis l'usage ledger.
- Bandeau timeline en pied : barres proportionnelles à la durée par agent, palette du thème actif.
- Navigation : h/l entre stages, j/k entre cartes, Enter = fiche (lecteur existant), `x` cancel, `p` pause stage, `s` steer (ouvre le composer ciblé), `g` approve gate, `q` retour.
- Discipline visuelle existante : glyphes kanji 2 cellules, `display_width` partout — **les 5 sites chars-vs-cellules du backlog sont corrigés dans ce chantier** (goal_badge, truncate_tool_name, truncate_for_modal, forge_sheet_title, wrap_words).
- Données : `subagent_snapshot` + notifications (existants) + événements workflow S3. Zéro nouvelle dépendance.

## Ordre de livraison

1. **S2 web** (valeur immédiate, petit, indépendant) → 2. **S1 vision** (petit) → 3. **S3 workflows** (le gros) → 4. **S4 mission-control** (consomme les events S3).

## Non-buts v1

Rendu d'image inline terminal (kitty/sixel) ; dashboard web (P4, mêmes events WS) ; rejeu orchestré de l'arbre workflow complet ; robots.txt ; provider-native search là où l'intégration n'est pas triviale.

## S5 — Télémétrie tokens/coûts (ajout user 2026-09-05, recadré : natif kaji, pas d'export)

Objectif recadré par le user : un **suivi poussé de la consommation intégré à kaji** — kaji sera l'outil majeur quand les abonnements disparaîtront et qu'il ne restera que les clés API : chaque token compte en argent réel. Prometheus n'était qu'un exemple, PAS un livrable — tout vit dans kaji. Fondation : `usage_ledger` (SQL), `usage_windows/usage_since`, `/cost`.

- **Dimensions** : par modèle, par provider, par session, par projet (`working_dir` racine git), par jour/semaine/mois calendaires (locale) en plus des fenêtres glissantes 5 h/7 j.
- **Économie du cache** : les colonnes `cache_read/cache_write` existantes remontent en 1re classe — taux de hit, $ économisés vs $ pleins — c'est LE levier de coût en régime clés API.
- **Burn rate & projection** : coût du jour/de la semaine en cours + projection fin de mois par régression simple ; affiché dans `/cost` et disponible en JSON.
- **Budgets** : `KAJI_BUDGET_MONTHLY_USD` (global et par provider, config) → ligne d'avertissement TUI aux seuils 50/80/100 % (pattern quota-awareness ccusage du user), jamais un hard-stop.
- **Surfaces** : `/cost` gagne les vues `modèles | jour | semaine | mois | cache | projection` (tableaux thème actif) ; CLI `kaji metrics [--window ...] [--by model|provider|session|project] [--format json|table]` pour scripts et cron.
- **Non-but** : exporteur Prometheus/OTel (un `--format json` suffit à qui veut brancher autre chose plus tard).

## S6 — Hooks de cycle de vie (ajout user 2026-09-05, « oui » Task 10)

Objectif : le dernier bloqueur du remplacement complet de Claude Code — l'outillage perso (rtk-rewrite, adhd-contract, shosoin-context) vit dans les hooks. kaji gagne des hooks shell aux mêmes points de vie, sémantique compatible dans l'esprit.

- **Événements v1** : `session_start`, `user_prompt_submit`, `pre_tool_use`, `post_tool_use`, `turn_end`, `session_end`.
- **Config** : `hooks:` dans la config user + `.kaji/hooks.yaml` par projet (merge user→projet) ; chaque hook = `{event, command, timeout_s (défaut 10), matcher?}` (matcher = nom d'outil pour pre/post_tool_use).
- **Contrat d'exécution** : payload JSON sur stdin (event, session_id, tool_name/args le cas échéant, prompt) ; exit 0 → stdout injecté (contexte pour session_start/user_prompt_submit, feedback pour post_tool_use) ; exit ≠ 0 sur `pre_tool_use` → **appel bloqué**, stderr = raison montrée au modèle ; `user_prompt_submit` peut réécrire (stdout remplace/annote le prompt — c'est le cas rtk). Timeout → hook ignoré + ligne système (fail-open sauf pre_tool_use : fail-closed documenté).
- **Règle replay (AGENTS.md)** : tout stdout de hook qui entre dans le prompt est de l'état externe → journalisé (kind `hook_output`, adressage par tour/appel) et **servi au rejeu sans ré-exécuter les hooks** — un rejeu n'exécute jamais de commande hook.
- **Parité** : sites partagés ou double application legacy/SM, prouvée par tests.
- **Tests d'acceptation** (les 3 usages réels) : un hook session_start qui injecte du contexte ; un user_prompt_submit qui préfixe le prompt ; un pre_tool_use qui bloque `shell` sur un pattern et laisse passer le reste.
