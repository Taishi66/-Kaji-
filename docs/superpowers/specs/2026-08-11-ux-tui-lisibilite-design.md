# Spec — UX TUI : lisibilité, tableaux, graphiques, concision

Statut : approuvée (4 blocs validés en session 2026-08-11, capture d'écran à l'appui).
Constat : tables markdown affichées en pipes bruts, lignes pleine largeur (~200 col)
illisibles, inline-code visuellement agressif, réponses modèle verbeuses.

## Bloc A — Scroll (livré séparément, hors de cette spec)

Flèches ↑↓ (= molette via traduction alt-screen des terminaux) + borne mesurée au rendu.

## Bloc B — Markdown v2 (crates/kaji-cli/src/tui/markdown.rs + ui.rs + theme.rs)

1. **Tables pipe rendues** : séquence de lignes `| a | b |` (+ ligne séparatrice `|---|---|`)
   → tableau box-drawing aligné (┌─┬─┐ │ ├─┼─┤ └─┴─┘), largeurs de colonnes naturelles
   (max du contenu), budget total 100 colonnes — cellules tronquées avec `…` au-delà.
   Table malformée (nombre de colonnes incohérent) → rendu brut inchangé (fallback sûr).
   Inline (gras/code) DANS les cellules : v1 texte brut stylé uniforme (pas de nested spans).
2. **Largeur de lecture plafonnée** : le chat est rendu dans un sous-rect de largeur
   `min(inner.width, 102)` (ui.rs draw_chat) — le wrap et `wrapped_rows` utilisent cette
   mesure ; sur terminal ultra-large le texte ne court plus sur 200 colonnes.
3. **Inline-code adouci** : style code = teinte de fond discrète + fg accent (theme.rs),
   plus de bloc inversé agressif.

## Bloc C — Graphiques terminal (markdown.rs)

Bloc fencé `kaji-chart` contenant du JSON minimal :

```json
{ "type": "bar" | "pie", "title": "optionnel", "items": [ { "label": "x", "value": 42.0 } ] }
```

- `bar` : une ligne par item — `label  ██████████████  42` (barres proportionnelles au max,
  largeur de barre budget ~40 col, labels alignés à gauche, valeurs à droite).
- `pie` : proportions — `label  ████████  38 %` (barres % du total + pastille `●` colorée,
  rotation de 4 couleurs du thème). Assumé : pas de rond dessiné (illisible en cellules).
- JSON invalide / items vides / valeurs négatives → bloc affiché brut (fallback sûr, jamais
  de panic). Parse via serde_json si déjà dep de kaji-cli, sinon parseur minimal — pas de
  nouvelle dépendance.

## Bloc D — Directive de style (crates/kaji/src/prompts/system.md)

Section courte ajoutée au prompt système principal : réponses scannables et concises ;
tableau markdown d'office dès qu'un comparatif/une énumération chiffrée s'y prête ;
bloc ` ```kaji-chart ` (format ci-dessus) dès que des proportions ou comparaisons
numériques sont plus parlantes en graphique ; pas d'explications à rallonge.
Conséquence : les tests snapshot du prompt (crates/kaji/src/agents/snapshots) doivent être
régénérés — même procédé que le rebrand identité (`f55c0fe61`).

## Non-objectifs

Rendu riche des cellules de table (nested inline) ; mouse capture ; sparklines/lignes ;
thème configurable des charts ; largeur de mesure configurable.

## Vérification

Tests unitaires markdown (table alignée exacte, malformée → brut, chart bar/pie exacts,
JSON invalide → brut) ; tests ui (largeur plafonnée dans wrapped_rows) ; snapshots prompt
régénérés ; baseline kaji-cli verte ; E2E visuel tmux (table + chart réels affichés).
