# Spec — TUI : palette de commandes « / » (lazyvim-like)

Statut : approuvée (design validé en session 2026-08-11 — position + interaction choisies
sur mockups). Extension T5 du plan UX v3.
Scope : crates/kaji-cli uniquement (app.rs, ui.rs, theme.rs). Aucun changement agent-loop.

## Cycle de vie

- La palette s'ouvre dès que l'input **commence par `/`** — quelle qu'en soit l'origine
  (frappe directe, rappel d'historique, Tab). Elle se ferme quand : Enter (exécution),
  **Esc (annulation : vide l'input)**, ou l'input ne commence plus par `/`.
- Overlay ancré **au-dessus de la zone d'input**, rendu après le chat (comme les modals) :
  aucun impact sur la mesure `chat_overflow`/`user_turn_rows`.
- Priorités : modal y/n actif (tool approval / gate) → la palette ne s'ouvre pas et ses
  touches ne s'appliquent pas. Palette visible → **Esc ferme la palette, n'interrompt
  jamais le tour en cours** ; ↑/↓ naviguent la palette (jamais l'historique ni le scroll).

## Interaction

- **Filtre préfixe** en live sur le nom (`/s` → `/sdd`, `/spec`). Pas de fuzzy (7
  commandes). Sélection par défaut : premier item ; le filtre qui change resélectionne le
  premier.
- **↑/↓** : déplacement **cyclique** de la sélection.
- **Enter** : exécute la commande **sélectionnée** (pas le texte tapé). Filtre sans aucun
  match → la palette est fermée de fait (liste vide non affichée) et Enter soumet le texte
  tel quel (comportement actuel, message envoyé à l'agent).
- **Tab** : complète l'input avec la commande sélectionnée, sans exécuter.

## Source unique des commandes

Table `COMMANDS` (nom, description courte, action) dans app.rs — le dispatch du submit,
`/help` et la palette en dérivent tous trois. Supprime la triplication actuelle
(if/else du dispatch, texte de /help, welcome) qui pouvait diverger.

## Rendu

- Bordures arrondies `╭╮╰╯`, titre `commandes`, footer dim `↑↓ choisir · ⏎ valider · esc`.
- Item sélectionné : marqueur `▸` + nom en VERMILLON ; autres noms en style normal ;
  descriptions en dim, alignées.
- Largeur : bornée au mockup validé (~ largeur du plus long item + description, clampée à
  la largeur de l'input). Hauteur : nb d'items filtrés + bordures, clampée à l'espace
  au-dessus de l'input (terminal bas → liste tronquée, la sélection reste visible).
- Liste vide (aucun match) → pas de palette du tout (pas de boîte vide).

## Non-objectifs

Fuzzy matching ; commandes avec arguments ; palette pour les raccourcis clavier
(F2/F3/Ctrl+↑↓ restent dans /help) ; souris dans la palette (clic/hover) ; persistance
d'un MRU de commandes.

## Vérification

Tests App : ouverture sur `/`, fermeture (Esc vide l'input, Esc ne renvoie pas
l'interruption du tour, backspace jusqu'à vider), filtre préfixe, navigation cyclique,
Enter exécute la sélection (ex. sélection `/think` → toggle + input vidé), Enter sans
match soumet tel quel, Tab complète sans exécuter, priorité sur historique et scroll,
modal y/n bloque la palette. Tests rendu (TestBackend) : palette visible et filtrée avec
`/s`, absente sans `/`, absente quand aucun match. `/help` et le dispatch dérivés de
`COMMANDS` (test d'exhaustivité : chaque commande de la table est dispatchée).
