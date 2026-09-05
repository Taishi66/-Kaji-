# Plan P3 — Vision, Web natif, Workflows, Mission-control

Spec (autorité) : `docs/superpowers/specs/2026-09-05-p3-vision-web-workflows-mission-control-design.md` (S1-S4, décisions user 2026-09-05). BASE : HEAD au dispatch de T1 (après la vague follow-ups replay). Exécution : dispatches séquentiels Fable-orchestre, implémenteurs Opus, reviews Sonnet (**Opus pour T1 — SSRF/sécurité — et la review finale whole-branch**).

## Global Constraints

- cargo TOUJOURS foreground, `timeout: 600000` explicite, JAMAIS run_in_background/Monitor/`&`, un seul cargo à la fois ; bascule background au timeout → boucler foreground sur `ps aux | grep -cE "[c]argo|[r]ustc"` jusqu'à 0 puis relancer. Filtres ciblés en itération, suites complètes une fois en fin de task.
- Formatage : `rustfmt --edition 2021 <fichiers touchés>` (rustfmt.toml pinne style 2021, repo fmt-clean depuis P1) ; jamais `cargo fmt` avec des fichiers non liés sales dans l'arbre.
- FOREIGN interdits tant que non commités : `crates/kaji-cli/src/tui/{app,mod,theme,ui,viewer}.rs`, `crates/kaji-exec/` (préexistants, étrangers).
- git add explicite par commit, `git diff --stat --cached` avant commit. `source bin/activate-hermit`. `/bin/rm -f`. TDD strict (test rouge d'abord). Un commit par task (ou par finding en fix wave).
- **Règle replay (AGENTS.md)** : toute nouvelle source d'état externe entrant dans le prompt exige son kind v2 capturé + servi — T4 la applique dès la conception ; T1/T2 vérifient que leurs sorties passent par les chemins déjà journalisés (`tool_result`, `message`).
- Parité legacy/SM par task : site partagé ou double application, dit au rapport.
- Échecs préexistants tolérés (compaction ×2, codex, claude_code providers, flake opening_message SM) — zéro nouveau.

## Task 1 — Extension web (S2) — review sécurité Opus

Create `crates/kaji-mcp/src/web/` (ou platform extension selon le patron `tom`) : outils `web_search{query,count}` + `web_fetch{url,mode}` ; trait `SearchBackend` + impls `brave`/`tavily`/`searxng` (config `KAJI_WEB_SEARCH_BACKEND` + clé/URL) ; fetch reqwest timeout 30 s, cap 2 Mo, redirects ≤5, extraction markdown ; **garde SSRF** (DNS→refus IP privées/loopback/link-local, refus `file://`, opt-out `KAJI_WEB_ALLOW_PRIVATE=1`), UA `kaji/<version>`. Tests : backends mockés (serveur local), garde SSRF (cas IP privée/loopback/redirect vers privé), extraction. Commit `feat(web): extension web_search/web_fetch — backends Brave/Tavily/SearXNG, garde SSRF`.

## Task 2 — Vision : images attachées (S1)

Modify `crates/kaji-cli/src/tui/mentions.rs` (+ composer path) : mention à extension image → attachement `MessageContent` image base64+mime, placeholder `画 nom (taille)`, caps 5 Mo / 3 par message, formats png/jpg/webp/gif. Vérifier le rendu des blocs image côté provider (Anthropic/OpenAI/Bedrock) et l'erreur propre si provider sans vision. Test : mention image → contenu multimodal dans le message construit ; cap dépassé → ligne d'erreur, message sans image ; replay d'une session avec image (doré étendu si léger). Commit `feat(vision): images attachées via @mention — multimodal provider`.

## Task 3 — Workflows : spec YAML + parser (S3)

Create `crates/kaji-core/src/workflow/spec.rs` (stdlib+serde) : structs `WorkflowSpec{stages}`, `Stage{agents, depends_on, gate, budgets}`, `AgentSpec{recipe|prompt, model, inputs}` ; validation (DAG acyclique, refs d'inputs résolubles, budgets positifs) ; erreurs nommées. Tests table-driven parse+validation. Commit `feat(workflow): spec YAML — stages, fan-out, gates, budgets`.

## Task 4 — Workflows : exécuteur + events v2 (S3)

Create `crates/kaji/src/workflow/` : ordonnanceur DAG au-dessus de summon (états Pending/Running/Waiting/Done/Failed/Cancelled, artefacts nommés, budgets → coupure propre) ; kinds v2 `workflow_started/stage_started/agent_started/agent_done/gate_decision/workflow_done` **capturés ET servis au replay** (gates depuis le log en rejeu) ; `parent_session_id` sur les sessions d'agents. Tests : DAG 2 stages fan-out avec provider fixture ; gate approve rejouée depuis le log ; budget coupé. Commit `feat(workflow): exécuteur DAG sur summon, events v2 rejouables`.

## Task 5 — Workflows : CLI + contrôle (S3)

`kaji workflow run <file>|list|status` (patron `replay.rs`) ; contrôle live branché sur l'exécuteur : pause/reprise stage, cancel agent, steer (message injecté), approve/deny gate — exposé via l'API que T6 consommera. Tests : run bout-en-bout fixture, codes de sortie, status. Commit `feat(workflow): CLI run/list/status + contrôle live`.

## Task 6 — Mission-control : vue plein écran (S4)

Create `crates/kaji-cli/src/tui/missioncontrol.rs` : vue plein écran depuis le volet forge (`/forge full`), colonnes = stages (« libre » hors workflow), cartes agent 3 lignes (遣 nom, statut+lame, outil courant, `炭 in↑ out↓ · $ · durée` via usage ledger), bandeau timeline proportionnel, palette du thème actif, glyphes 2 cellules. **Corrige les 5 sites chars-vs-cellules** (goal_badge, truncate_tool_name, truncate_for_modal, forge_sheet_title, wrap_words) via `gitstatus::display_width`. Tests TestBackend : layout, troncatures cellules, données live. Commit `feat(tui): mission-control plein écran + fixes chars-vs-cellules`.

## Task 7 — Mission-control : interactions (S4)

h/l stages, j/k cartes, Enter fiche (lecteur), `x` cancel, `p` pause, `s` steer (composer ciblé), `g` gate, `q` retour ; matrice de précédence des touches vérifiée sous overlays (leçon review finale ante : Shift+Tab sous dropdown). Tests App par touche + gating overlays. Commit `feat(tui): contrôle live du mission-control`.

## Task 8 — Dorés + gate finale

Doré workflow : record run 2-stages → `kaji replay` de la session parente (gates du log) + d'un agent enfant, sans divergence ×2 ; doré vision (image dans le tour) ; self-test yaml : `kaji workflow run --help` exit 0, `web_search` surface (sans réseau : backend absent → erreur nommée). Gate : `cargo build`, `cargo test -p kaji-core -p kaji -p kaji-cli -p kaji-mcp`, `cargo clippy --workspace --all-targets -- -D warnings` (touch des fichiers modifiés avant — piège diagnostics en cache), zéro nouveau failure. Commit `test(p3): dorés workflow-replay et vision ; self-test`.

## Après chaque task

Review (Sonnet ; **Opus T1 + finale**), fix rounds bornés, commit ; `just install` + push en fin de séquence ; notes vault (Règle 12) ; ledger `.superpowers/sdd/2026-09-05-p3/progress.md` (créer au 1er dispatch, 1re ligne = chemin de ce plan).

## Task 9 — Télémétrie tokens (S5)

Vérifier le schéma `usage_ledger` (le modèle est-il déjà par ligne ? sinon migration additive) ; agrégats par modèle + fenêtres calendaires jour/semaine/mois ; `/cost` vues `modèles|jour|semaine|mois` ; CLI `kaji metrics` (json/table) ; export Prometheus : `/metrics` sur `kaji serve` + `--prometheus` one-shot (format exposition texte, zéro dépendance). Tests : agrégats sur ledger fixture, format exposition conforme, fenêtres calendaires aux bornes (minuit, lundi, 1er du mois). Commit `feat(metrics): tokens/coûts par modèle, fenêtres calendaires, export Prometheus`.
