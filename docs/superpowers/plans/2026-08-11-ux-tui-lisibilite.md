# UX TUI Lisibilité Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rendu markdown lisible quel que soit le terminal (tables réelles, mesure plafonnée, code adouci), graphiques `kaji-chart`, directive de concision — spec `docs/superpowers/specs/2026-08-11-ux-tui-lisibilite-design.md`.

**Architecture:** Tout le rendu vit dans `crates/kaji-cli/src/tui/markdown.rs` (281 lignes, fonctions par élément : `render_markdown` → `render_heading`/`render_blockquote`/`render_list_item`/`render_code_line`/`render_inline_spans`, mod tests riche) + `ui.rs` (mesure) + `theme.rs` (styles). La directive de style vit dans `crates/kaji/src/prompts/system.md` (snapshots à régénérer).

**Tech Stack:** Rust, ratatui `Line`/`Span`, serde_json (vérifier qu'il est déjà dep de kaji-cli — sinon parseur minimal maison, pas de nouvelle dépendance).

## Global Constraints

- Fallback sûr partout : table malformée → lignes brutes inchangées ; JSON chart invalide/vide/négatif → bloc brut ; JAMAIS de panic sur entrée arbitraire (le LLM écrit ce qu'il veut).
- Budget largeur table = 100 colonnes (troncature cellule avec `…`) ; mesure chat = `min(inner.width, 102)` ; barres chart ≤ 40 colonnes.
- v1 cellules de table : texte stylé uniforme, pas de nested inline (non-objectif).
- Baseline : kaji-cli 346/346 + nouveaux ; clippy scoped + workspace verts ; `cargo fmt`.
- Un commit par tâche, message `kaji: tui — …` (T-D : `kaji: prompts — …`) + trailer `Claude-Session: https://claude.ai/code/session_014ngoE4sNSgzrZPdgb7qC2r`.
- Cargo foreground, un seul à la fois.

---

### Task 1: Tables + mesure + code adouci (Bloc B)

**Files:**
- Modify: `crates/kaji-cli/src/tui/markdown.rs` (détection/rendu tables)
- Modify: `crates/kaji-cli/src/tui/ui.rs` (sous-rect largeur `min(inner.width, 102)` dans draw_chat — le Paragraph ET `wrapped_rows` utilisent cette largeur ; attention : `chat_overflow` du fix scroll `5288cb033` doit être mesuré avec la MÊME largeur)
- Modify: `crates/kaji-cli/src/tui/theme.rs` (style inline-code adouci : fond sombre discret + fg accent, plus d'inversion)

**Interfaces:**
- Produces: `render_markdown` inchangé de signature ; nouvelle fn interne `try_render_table(lines: &[&str]) -> Option<Vec<Line<'static>>>`.

- [ ] **Step 1 (RED)** : tests markdown.rs — `renders_pipe_table_as_box_drawing` (entrée 2 colonnes/2 lignes + séparateur → sortie exacte ┌─┬─┐/│/├─┼─┤/└─┴─┘ alignée, largeurs naturelles) ; `truncates_wide_table_cells_to_total_budget` (cellule de 200 chars → `…`, largeur totale ≤ 100) ; `malformed_table_falls_back_to_raw_lines` (colonnes incohérentes → lignes brutes) ; `table_inside_other_content_renders_between_paragraphs`.
- [ ] **Step 2** : run RED (`cargo test -p kaji-cli markdown`).
- [ ] **Step 3** : implémentation — dans `render_markdown`, bufferiser les lignes consécutives commençant par `|` ; à la rupture, `try_render_table` : parse cellules (`split('|')` trimé, ignorer premier/dernier vides), ligne 2 = séparateur (`-`/`:` uniquement) obligatoire, sinon None → brut. Largeurs = max par colonne (chars), réduction proportionnelle si somme+bordures > 100, troncature `…`. Style : bordures `theme::dim()`, header en gras, cellules texte brut.
- [ ] **Step 4** : ui.rs — sous-rect centré-gauche `Rect { width: inner.width.min(102), ..inner }` pour le Paragraph chat + `wrapped_rows`/`chat_overflow` calculés sur cette largeur. Test : `chat_measure_is_capped_at_102_columns` (si testable au niveau des fns pures — sinon vérifier par le calcul de `line_wrapped_rows` exposé).
- [ ] **Step 5** : theme.rs — adoucir `inline_code`/le style code (chercher son nom réel) ; ajuster les tests d'assertion de style existants.
- [ ] **Step 6** : GREEN complet (`cargo test -p kaji-cli`), clippy scoped, fmt.
- [ ] **Step 7** : commit `kaji: tui — tables markdown rendues, mesure de lecture plafonnée, inline-code adouci`.

### Task 2: kaji-chart (Bloc C)

**Files:**
- Modify: `crates/kaji-cli/src/tui/markdown.rs` (branche fence `kaji-chart`)
- Modify: `crates/kaji-cli/Cargo.toml` UNIQUEMENT si serde_json absent des deps (vérifier — probablement présent via workspace)

**Interfaces:**
- Consumes: la gestion de fence existante de `render_markdown` (les blocs fencés passent par `render_code_line` — brancher sur le language tag `kaji-chart`).

- [ ] **Step 1 (RED)** : tests — `renders_bar_chart_block` (JSON 3 items → 3 lignes `label  ███…  value`, barres proportionnelles au max, labels alignés) ; `renders_pie_chart_with_percentages` (somme → `38 %` etc., pastille `●`) ; `invalid_chart_json_falls_back_to_raw_block` ; `empty_items_falls_back` ; `negative_values_fall_back`.
- [ ] **Step 2** : run RED.
- [ ] **Step 3** : implémentation — au début d'un bloc fencé, si le tag == `kaji-chart`, accumuler le corps ; à la fence fermante, parser (serde_json si dep, struct `ChartSpec { r#type, title, items }`) ; rendu : titre optionnel stylé, par item `label` paddé à gauche (max label ≤ 24, tronqué `…`), barre `█` proportionnelle (bar : /max ; pie : /somme) largeur ≤ 40, valeur (bar) ou `NN %` (pie) à droite ; pie : pastille `●` colorée en rotation sur 4 couleurs du thème. Échec de parse à n'importe quelle étape → rendre le bloc comme code brut (comportement actuel).
- [ ] **Step 4** : GREEN, clippy scoped, fmt.
- [ ] **Step 5** : commit `kaji: tui — graphiques kaji-chart (barres et proportions unicode) dans le chat`.

### Task 3: Directive de concision (Bloc D)

**Files:**
- Modify: `crates/kaji/src/prompts/system.md`
- Modify: snapshots impactés sous `crates/kaji/src/agents/snapshots/` (régénération outillée, PAS d'édition manuelle : identifier le mécanisme — `cargo insta` ou env `UPDATE_SNAPSHOTS`/`INSTA_UPDATE` — en regardant comment le commit d'identité `f55c0fe61` les a mis à jour : `git show f55c0fe61 --stat`)

- [ ] **Step 1** : rédiger la section (concise, ~10 lignes, français ou langue du fichier — respecter la langue existante de system.md) : réponses scannables et concises ; tableau markdown dès qu'un comparatif/énumération chiffrée s'y prête ; bloc fencé `kaji-chart` (`{"type":"bar"|"pie","title":"…","items":[{"label":"…","value":N}]}`) dès que des proportions/comparaisons numériques sont plus parlantes en graphique ; pas d'explications à rallonge.
- [ ] **Step 2** : `cargo test -p kaji --lib` filtré sur les tests prompt/snapshot → identifier les échecs de snapshot, régénérer par l'outil, vérifier le diff des snapshots (la section apparaît, rien d'autre ne bouge).
- [ ] **Step 3** : baseline `kaji --lib` : uniquement les 8 échecs préexistants ; clippy scoped, fmt.
- [ ] **Step 4** : commit `kaji: prompts — directive réponses scannables (tableaux d'office, kaji-chart pour les proportions)`.

---

## Self-review (à la rédaction)

- Spec ↔ tasks : B→T1, C→T2, D→T3, A hors scope (livré `5288cb033`) ✓. Fallbacks sûrs dans chaque étape RED ✓. Types cohérents (`try_render_table` interne, pas d'API inter-tâches) ✓. Interaction T1×fix scroll (chat_overflow mesuré sur la largeur plafonnée) explicitement notée ✓.
