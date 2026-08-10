# TUI UX v2 — spec design « forge » (drastique)

Demande utilisateur (2026-08-09) : « amélioration drastique de l'UX UI pour que ce soit ultra esthétique et ultra pratique avec les infos nécessaires ». Constats v1 : markdown affiché brut, appels outils opaques (`· ⚙ shell` ×N sans détail ni durée), aucune notion du temps qui passe, pas de scroll, panneau SPEC surdimensionné quand vide, aucune info session/provider/tokens.

Scope : `crates/kaji-cli/src/tui/` uniquement (client pur — zéro changement dans `crates/kaji`). Part APRÈS l'atterrissage du chantier branding/UX-v1 (mêmes fichiers).

## Thème « zen japonais × forge » (constantes dans un module `theme.rs`)

Directive utilisateur : **zen japonais** — sobriété wabi-sabi, beaucoup d'air, ornement minimal — et le nom **鍛冶** (kaji, « forgeron ») doit apparaître.

- Accents (rares, jamais criards) : encre sumi `Rgb(200,200,195)` (texte), indigo ai 藍 `Rgb(84,110,140)` (préfixe user, bordures inactives), vermillon shu 朱 `Rgb(203,88,65)` (accent actif : étage ▶, spinner, alerte), or patiné `Rgb(196,164,106)` (titres, 鍛冶, ✓). Fond = défaut terminal (jamais repeint). Bordures fines simples, pas de double-trait.
- Préfixes messages : user `vous ▸` indigo · agent `鍛冶 ▸` or patiné gras · système `·` gris dim italique · une ligne vide entre messages — le blanc fait partie du design.
- Symbole d'étape/section : `◦` ; gate/passe : `⚔` conservé avec parcimonie.

## Header (1 ligne, pleine largeur)

`鍛冶 kaji · <session_id> · <provider>/<modèle>` à gauche (鍛冶 en or patiné gras, le reste dim) ; à droite : `↑<input_tok> ↓<output_tok> · <elapsed>s` pendant un tour, cumul tokens hors tour. Tokens : consommer `AgentEvent::MessageUsage`/`Usage` dans `apply_agent_event` (vérifier les champs réels du type usage avant : `crates/kaji/src/agents/agent.rs:277-286` et le type `MessageUsage`).

## Chat

- **Markdown rendu** : renderer maison pur (`markdown.rs`, fn `render_markdown(&str) -> Vec<Line>`) — titres (# gras souligné or), **gras**, *italique*, `code inline` (fg braise sur fond sombre/reverse), blocs ``` (indentés, bordure gauche `│` dim, pas de wrap), listes (`•`/`1.` indentés), blockquote (`▎` dim). PAS de nouvelle dépendance sans vérifier la compat ratatui 0.30 (si `tui-markdown` compatible et propre, autorisé ; sinon maison). Tests unitaires du renderer (gras, code inline, bloc code, liste, texte mixte).
- **Scroll** : PageUp/PageDown (page), flèches Haut/Bas quand l'input est vide ? Non — conflit historique de saisie : PageUp/PageDown + Home/End seulement. État `scroll_offset` dans App ; auto-collé en bas quand offset=0 ; indicateur `▼ …` dans le titre du box quand on n'est pas en bas. Tests App du scroll (offset borné).
- **Appels outils** : une ligne par appel — `⚙ <nom> ⠋` (spinner braille animé) pendant l'exécution → remplacée par `✓ <nom> (<durée>s)` à la réponse. Pairing : tracker les `ToolRequest.id` en attente dans App (map id→(nom, Instant)) ; les `ToolResponse` arrivent en messages rôle User (`Message::user().with_tool_response`) — étendre `apply_agent_event` pour matcher ces blocs SANS rendre le contenu du tool result. Test App : request→response remplace la ligne et affiche ✓.

## Rythme / temps

- Branche `tokio::time::interval(250ms)` dans le `select!` : tick → redraw (spinner + elapsed vivants) uniquement quand `turn_active`.
- `App.turn_started: Option<Instant>` posé au Submit/relance, cleared à la fin.

## Input

- Titre « message » or ; placeholder dim « écris ici… » ; pendant un tour : titre `⏳ <elapsed>s — Esc annule`. Statut hors box input (titre du chat ou header).

## Panneau SPEC

- Aucune SPEC et aucune passe : panneau MASQUÉ, chat pleine largeur ; `/spec` (ou F2) le montre/cache. Avec spec ou passe active : 28 %, étages avec symboles colorés (✓ or, ▶ braise, ✗ rouge, · dim).

## Modale gate

- Bordure or patiné, `⚔ Gate — approuver la SPEC ? (y/n)`, fond Clear.

## Approbation d'outils (trou de la v1, requis)

La TUI ignore les blocs `ActionRequired` : en `KajiMode::Approve`/`SmartApprove` (défaut = `Auto`, `crates/kaji-provider-types/src/kaji_mode.rs:22-32`), un tour attendrait une confirmation jamais affichée. Requis : détecter les blocs `ActionRequired` dans `apply_agent_event`, afficher une modale y/n (même pattern que la gate) et répondre à l'agent par le même mécanisme que le CLI — étudier comment `session/mod.rs` répond aux confirmations d'outil (grep `ActionRequired`/`confirmation`/`permission` dans `crates/kaji-cli/src/session/mod.rs:1347-1524`) et répliquer. Si le mécanisme exact s'avère trop couplé au CLI pour cette passe : à minima afficher « ⚠ confirmation d'outil requise — relance en mode Auto (kaji configure) » au lieu du silence.

## Accueil

- Bloc d'accueil v1 conservé (posé par la boucle, pas App::new) + `/help` qui réaffiche les touches.

## Interdits

- Aucun changement de comportement agent/driver SDD au-delà du tracking outils/usage/scroll décrits.
- Chaînes tests protégées : ne pas réutiliser « tour en cours », « VERDICT », « passe SDD complète », « compact » dans les nouveaux textes.
- Tests App existants (17+) doivent rester verts (adapter uniquement si une assertion porte sur un texte déplacé).
