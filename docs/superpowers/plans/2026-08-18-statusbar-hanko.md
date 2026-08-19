# Barre d'état « hanko & forge » — Implementation Plan

> Contrôleur : Fable (planifie, review, rulings). Exécuteur : Opus. Design validé en session le 2026-08-18 (brainstorming bounded, deux tours d'aperçus) : direction « hanko & forge », sceau kanji seul.

**Goal:** Remplacer la barre d'état lualine (Task 15 du plan `2026-08-15-ante-restants.md`) par une barre zen, novatrice et pertinente : un sceau vermillon (hanko) qui porte le kanji du mode, le lieu (在 dossier ⟩ branche + état git) en encre pâle, du vide au centre (間 ma), et à droite la télémétrie de la forge — modèle, 炭 tokens (charbon), coût en or, 火 + lame animée pendant un tour. Le header perd sa colonne de télémétrie (plus de doublon).

**Architecture:** TUI Ratatui in-process, `crates/kaji-cli/src/tui/`. Barre dessinée par `ui.rs::draw_status_bar` (ligne du bas, `root[2]`), partie git par `gitstatus.rs::render`, header par `ui.rs::draw_header` (+ `header_status_text`), header construit par `mod.rs::build_header`, styles/glyphes dans `theme.rs`, badge mode dans `app.rs::kaji_mode_badge`.

**Tech Stack:** Rust 2021, ratatui 0.30 (crossterm), tests unitaires en fin de module (`rendered(&app, w, h)` dans `ui.rs`, `rendered(status, dir, width)` dans `gitstatus.rs`).

## Global Constraints (valables pour toutes les Tasks)

- **cargo TOUJOURS foreground**, `timeout: 600000` explicite sur CHAQUE commande cargo, JAMAIS `run_in_background`, JAMAIS `Monitor`, JAMAIS `&`. Un seul cargo à la fois. Si l'outil Bash bascule quand même en background au timeout : NE PAS attendre la notification — boucler en petits appels foreground `ps aux | grep -cE "[c]argo|[r]ustc"` jusqu'à `0`, puis relancer la commande (elle sera incrémentale).
- `source bin/activate-hermit` avant tout cargo (hermit fournit la toolchain).
- Formatage : **le repo est en style mixte**. Règle : **jamais `cargo fmt`** (reformate tout le workspace, même avec `-- <fichier>`). Par fichier touché : `rustfmt --edition 2021 --style-edition 2024 --check <f>` puis `--style-edition 2021 --check <f>` → appliquer le style qui laisse le fichier propre ; si les deux créent du churn hors de tes hunks (cas connu : `tui/mod.rs`) → ne pas lancer rustfmt dessus, formater tes hunks à la main dans le style environnant. Après formatage, `git diff --stat` ne doit contenir que tes changements logiques ; tout reformatage de lignes non touchées est à annuler avant commit.
- Clippy scoped : `cargo clippy -p kaji-cli --all-targets -- -D warnings`.
- Tests : `cargo test -p kaji-cli --lib [filtre]` en itération ; suite complète `cargo test -p kaji-cli --lib` une fois avant commit.
- Règles repo AGENTS.md : code auto-documenté, zéro commentaire qui paraphrase, `anyhow::Result`, pas de code défensif inutile, pas de logs ajoutés hors erreurs. Les doc-comments existants du dossier `tui/` sont en anglais ou en français selon le fichier — suivre le fichier.
- Commit : `git add <fichiers touchés>` explicites (jamais `git add -A`), message en français, sujet `feat(tui): …`. Ne jamais commiter de fichiers hors périmètre (pas de `.superpowers/`, pas de `docs/superpowers/plans/`).
- Zéro subagent côté exécuteur.

---

## Task 1 : Barre d'état « hanko & forge » + header allégé — demande user 2026-08-18

**Contexte.** La barre actuelle (`ui.rs::draw_status_bar`) rend à gauche `在 ~/dir · branche · ↑↓ · ●✚… · +−` (spans de `gitstatus::render`, séparateur `" · "`, branche en `theme::title()`, ● et − en `theme::accent()`) et à droite le mot du mode (`app::kaji_mode_badge` : `auto`/`approve`/`smart`/`chat`, `theme::dim()`). Le header (`ui.rs::draw_header`) rend `鍛冶 kaji · {session_id} · {provider}/{model}` + badge goal à gauche et `header_status_text` (`↑in ↓out [· {elapsed}s] [· $cost]`) à droite dans une colonne `Length(34)`. `mod.rs::build_header(session_id)` fabrique la chaîne header via `Config::global().get_kaji_provider()/get_kaji_model()`. Le user veut une barre **novatrice, esthétique, zen** et **pertinente** — design validé ci-dessous.

**Cible visuelle** (tour actif, 130 colonnes) :

```
 自  在 ~/workspace/kaji ⟩ feat/kaji-init  ✚3 …2  +40 −12                     claude-fable-5  炭 120↑ 340↓  $0.42  火 ▋ 12s
```

Au repos (aucun tour) :

```
 自  在 ~/workspace/kaji ⟩ feat/kaji-init  ✓                                             claude-fable-5  炭 12k↑ 4.1k↓  $1.30
```

Hors dépôt git (`git_status == None`) : `在 ~/dossier` seul à gauche du vide, comme aujourd'hui.

**Spécification, de gauche à droite.**

1. **Sceau (hanko).** Texte `" {kanji} "` (espace, kanji, espace — 4 cellules), style `theme::seal()` = `Style::default().fg(palette.accent).add_modifier(Modifier::REVERSED | Modifier::BOLD)` (REVERSED : le fond prend la couleur accent, le texte prend la couleur de fond du terminal — lisible en thème clair comme sombre, aucune couleur de texte codée en dur). Toujours vermillon, quel que soit le mode : un hanko est rouge, c'est le kanji qui porte le sens. Kanji par mode, nouvelle fn `app::kaji_mode_seal(mode: KajiMode) -> &'static str` à côté de `kaji_mode_badge` (qui reste, utilisé par les messages `mode : …`) : `Auto → "自"`, `Approve → "承"`, `SmartApprove → "智"`, `Chat → "話"`. Puis un espace `" "` (style par défaut) avant le lieu. Le badge-mot disparaît de la barre (plus de colonne droite `auto`).
2. **Lieu.** `在 {dir}` en `theme::dim()` (glyphe `theme::DIR_GLYPH` existant + espace + `dir_label`), puis, si `git_status` est `Some` : `" ⟩ "` (nouvelle constante `theme::PLACE_SEPARATOR: &str = "⟩"`, entourée d'espaces, `theme::dim()`) puis la branche (`branch_label`) en `theme::text()` (plus `title()` : l'or est réservé au coût). Les groupes git suivants (upstream ↑↓, fichiers ●✚…/✓, diff +−) sont joints par **deux espaces** (`SEPARATOR` de `gitstatus.rs` passe de `" · "` à `"  "`) ; les compteurs perdent l'accent : ● staged `theme::text()`, ✚ `theme::text()`, … `theme::dim()`, + `theme::text()`, − `theme::text()`, ✓ `theme::dim()`, ↑/↓ `theme::text()`. Le lieu est fitté dans le budget restant par `truncate_left` (mécanisme existant de `gitstatus::render`, qui reçoit `width` = largeur disponible pour lui seul).
3. **Vide (間).** Tout l'espace entre le lieu et la télémétrie reste vide (style par défaut). Pas de fond, pas de bande : la page est le papier.
4. **Télémétrie (droite).** Éléments joints par **deux espaces**, dans l'ordre : `{model}` en `theme::dim()` (omis si `app.model` est vide) · `炭 {in}↑ {out}↓` en `theme::dim()` (nouvelle constante `theme::TOKENS_GLYPH: &str = "炭"`) — tour actif (`app.turn_active`) → `tokens_turn_in`/`tokens_turn_out`, repos → `tokens_total_in`/`tokens_total_out` (même sémantique que l'ancien `header_status_text`) · `${cost:.2}` en `theme::gold()` sur `app.cost_total` (omis si `None`) · **uniquement pendant un tour actif** : `火 {lame} {elapsed}s` en `theme::accent()` (nouvelle constante `theme::FIRE_GLYPH: &str = "火"`, lame = `theme::blade_frame(elapsed)` où `elapsed = app.turn_started.map(|t| t.elapsed()).unwrap_or_default()`, secondes entières). Au repos : rien — le silence est l'information. Un espace final de marge à droite (comme l'ancien `+ 1` du badge).
5. **Compteurs compacts.** Nouvelle fn `pub fn compact_count(n: i64) -> String` : `0..=999 → "987"` ; `1_000..=9_999 → "1.2k"` (une décimale, tronquée pas arrondie : `1_249 → "1.2k"`, `1_250 → "1.2k"`, `9_999 → "9.9k"`) ; `10_000..=999_999 → "12k"`, `"123k"` (entier, tronqué) ; `1_000_000..=9_999_999 → "1.2M"` ; au-delà `"12M"`, `"123M"`. Négatif : traiter comme `0` n'est pas nécessaire — les compteurs sont des i64 ≥ 0 en pratique ; ne pas coder de branche défensive, `n.max(0)` suffit si tu veux protéger le format.
6. **Largeur étroite (dégradation, jamais de débordement).** Calculer la télémétrie complète ; si `area.width < seal_width + MIN_PLACE_WIDTH + telemetry_width` avec `MIN_PLACE_WIDTH = 24`, la recalculer sans le modèle ; si toujours trop étroit, sans télémétrie du tout (le lieu prend tout, tronqué par `truncate_left`). Le sceau est toujours rendu. À 60 colonnes avec un chemin `~/workspace/kaji`, une branche et des compteurs, la ligne rendue tient dans 60 cellules et le sceau + `在` sont présents.
7. **Structure.** Créer `crates/kaji-cli/src/tui/statusbar.rs` (déclaré dans `tui/mod.rs` comme les autres modules : `mod statusbar;` — vérifier la forme utilisée pour `gitstatus`) exposant `pub fn render(app: &App, width: u16) -> Line<'static>` (ou une signature à champs explicites si tu préfères découpler des tests de `App` — au choix, mais une seule entrée). `statusbar.rs` contient : le sceau, l'assemblage lieu (délégué à `gitstatus::render`) / vide / télémétrie, `compact_count`, la dégradation, et **ses tests**. `ui.rs::draw_status_bar` devient un appel : `frame.render_widget(Paragraph::new(statusbar::render(app, area.width)), area)`. `gitstatus.rs::render` garde sa signature `(status: Option<&GitStatus>, dir: &Path, width: usize) -> Vec<Span<'static>>` et ses tests (adaptés aux nouveaux séparateurs/styles).
8. **Header.** `draw_header` ne garde que la colonne gauche : `鍛冶 kaji · {app.header}` + badge goal ; supprimer la colonne droite (`Length(34)`) et la fn `header_status_text` (et son import éventuel). `mod.rs::build_header` renvoie `session_id.to_string()` — plus de `provider/model` ; ajouter à `App` un champ `pub model: String` (initialisé `String::new()` dans `App::new`) alimenté au démarrage dans `mod.rs` au même endroit que `app.header = header;` avec `Config::global().get_kaji_model().unwrap_or_else(|_| "?".to_string())` (extraire une fn `model_label()` à côté de `build_header`). Ne pas afficher le provider dans la barre.
9. **`/help`.** Les deux lignes `Shift+Tab change le mode (approve → smart → auto)` de `mod.rs` (formes tableau et texte, ~lignes 273 et 298) deviennent `change le mode (承 approve → 智 smart → 自 auto) — sceau à gauche de la barre d'état` ; si un mode chat est sélectionnable par le user (vérifier `KajiMode::Chat` dans `app.rs`), ajouter ` · 話 chat` dans la parenthèse. Ne pas toucher au reste de `/help`.
10. **Thème.** Dans `theme.rs` : `pub fn seal() -> Style` (spec point 1), constantes `TOKENS_GLYPH`, `FIRE_GLYPH`, `PLACE_SEPARATOR` à côté de `DIR_GLYPH` (~ligne 218). Aucune couleur littérale hors palette.

**Tests (TDD : rouge → vert, preuves dans le rapport).**

- `statusbar.rs` : (a) le sceau rend `自` en mode Auto, `承` Approve, `智` SmartApprove, `話` Chat, et la ligne rendue ne contient plus le mot `auto`/`smart` ; (b) `compact_count` : `0 → "0"`, `987 → "987"`, `1_249 → "1.2k"`, `9_999 → "9.9k"`, `12_345 → "12k"`, `123_456 → "123k"`, `1_234_567 → "1.2M"`, `12_345_678 → "12M"` ; (c) tour actif → `火` et `炭` présents, tokens du tour affichés (`tokens_turn_in`), et `s` du chrono présent ; repos → pas de `火`, totaux affichés ; (d) `cost_total = Some(0.42)` → `$0.42` présent, `None` → aucun `$` ; (e) `model` vide → aucun modèle rendu, `model = "claude-fable-5"` → présent ; (f) largeur 60 avec dossier + `GitStatus{branch, modified: 3, ..}` : `Line::width() <= 60`, sceau + `在` présents, télémétrie absente ou sans modèle selon le calcul (assert précis sur ce que ton seuil donne) ; (g) largeur 130 : lieu, `⟩`, branche, télémétrie tous présents.
- `ui.rs` (tests existants à adapter, ne pas supprimer) : `the_status_bar_shows_the_working_directory_and_the_repository_state` — remplacer l'assert `bar.contains(app::kaji_mode_badge(..))` par `bar.contains(app::kaji_mode_seal(app.kaji_mode))` ; le test vers ligne 2276 qui asserte `bar.contains("smart")` → asserte le sceau `智` sur la barre et son absence dans le header ; ajouter un test : le header ne contient ni `↑` ni `$` ni le modèle (`app.model = "claude-fable-5"`), et la barre contient le modèle.
- `gitstatus.rs` : adapter les tests existants au séparateur `"  "` et à `⟩` (le helper `rendered()` concatène le texte des spans) ; vérifier qu'aucun test ne dépend de `" · "`.
- Suite complète `cargo test -p kaji-cli --lib` verte, clippy vert, rustfmt par fichier (règle Global Constraints — `mod.rs` : hunks formatés à la main).

**Hors périmètre.** Pas de jauge de contexte, pas de fond de bande, pas de changement des couleurs de palette, pas de modification des messages `mode : …`, pas de doc utilisateur (aucune doc CLI ne décrit la barre — vérifié 2026-08-18).

**Commit.** Un commit : `feat(tui): barre d'état hanko & forge — sceau kanji du mode, lieu ⟩ branche, 炭 tokens, coût en or, 火 lame pendant le tour ; header sans télémétrie`. Fichiers : `crates/kaji-cli/src/tui/{statusbar.rs,ui.rs,gitstatus.rs,theme.rs,app.rs,mod.rs}`.

**Rapport.** Sortie visuelle attendue : coller dans le rapport la ligne rendue (texte) à 130 colonnes en tour actif et au repos, telle que produite par le test (g), pour contrôle visuel par le contrôleur.
