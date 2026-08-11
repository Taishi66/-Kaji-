# Spec — UX TUI v3 : thinking, loader zen, souris, navigation, curseur ninja

Statut : approuvée (design v3 validé en session 2026-08-11, capture souris comme socle).
Scope : crates/kaji-cli uniquement (rendu/entrées TUI). Aucun changement agent-loop.

## Socle — capture souris (T3, conditionne les touches)

- `EnableMouseCapture` à l'init du terminal, `DisableMouseCapture` au restore (y compris
  chemins d'erreur). Kill-switch `KAJI_MOUSE` (défaut actif ; `0|false|FALSE|no` = off).
- Molette/trackpad → `MouseEventKind::ScrollUp/ScrollDown` → scroll chat de **3 lignes**
  par cran (pas naturel). Sélection de texte terminal : Option/Shift+drag (à documenter
  dans /help).
- Souris OFF (`KAJI_MOUSE=0`) : comportement flèches actuel conservé (scroll ligne à
  ligne), historique de prompts non accessible aux flèches — dégradation documentée.

## T1 — Padding + suppression de mot

- Chat : marge interne horizontale de 1 colonne (le texte ne colle plus aux bordures).
  La mesure (wrapped_rows/chat_overflow) suit la largeur réduite.
- Effacement de mot dans l'input : `Ctrl+Backspace`, `Option+Backspace` (ALT) et
  `Ctrl+W` (universel shell — certains terminaux confondent Ctrl+Backspace et Ctrl+H,
  d'où le triple mapping) → supprime les espaces de fin puis le dernier mot.

## T2 — Thinking toggle + loader zen

- Les blocs `Thinking` (aujourd'hui ignorés) sont accumulés par tour. État
  `show_thinking: bool`, **défaut OFF**, togglé par `/think` et **F3** (+ message système
  de confirmation). ON : rendu estompé italique, préfixe `思` — streaming au fil de l'eau
  comme le texte. OFF : rien d'affiché.
- **Loader zen** : visible quand un tour est en cours (`turn_pending || turn_active`) et
  qu'AUCUN contenu visible n'est encore arrivé pour ce tour (thinking masqué compris) :
  dernière ligne du chat = `{ensō} 思考中 · {N}s` — ensō animé sur les frames `◐ ◓ ◑ ◒`
  (tick 250 ms existant), style dim. Disparaît au premier chunk visible ou à la fin du
  tour. Pas de loader quand `show_thinking` ON et que du thinking s'affiche déjà.

## T3 — Navigation (souris + tours + historique)

- **Ctrl+↑ / Ctrl+↓** : saut de tour en tour dans le chat — positionne le début du
  message utilisateur précédent/suivant en haut de la vue. Les offsets de rangée wrappée
  de chaque message user sont mesurés au rendu (même mécanique/largeur que
  `chat_overflow` — `RefCell<Vec<u16>>` alimenté par draw_chat).
- **Historique de prompts** : chaque submit est empilé. `↑` (input vide ou navigation en
  cours) → prompt précédent dans l'input (rééditable) ; `↓` → suivant, ou vide l'input
  au bout ; toute édition (frappe/backspace) sort du mode navigation en gardant le texte.
  L'historique vit en mémoire de session (pas persisté) — v1.
- Les flèches nues ne scrollent PLUS le chat quand la souris est active (la molette s'en
  charge) ; PageUp/PageDown/Home/End inchangés.

## T4 — Curseur ninja

- Pendant le streaming du texte visible : une lame `▊` VERMILLON pulse au bout de la
  dernière ligne du message agent en cours (frames `▊ ▋ ▌ ▍` sur le tick), disparaît à
  la fin du tour. Ne s'applique ni aux lignes outils ni au chat historique.

## Non-objectifs

Édition mi-chaîne dans l'input (curseur ←/→) ; persistance de l'historique de prompts ;
clic souris (seul le scroll est mappé) ; thinking dans l'export/session (rendu seul).

## Vérification

Tests unitaires App (toggle, loader visible/invisible selon l'état, delete-word aux 3
mappings, historique ↑/↓ avec édition, saut de tours avec offsets simulés, molette ×3) ;
tests rendu markdown non touchés ; baseline kaji-cli verte ; E2E tmux (loader visible
pendant le blanc deepseek, /think affiche le raisonnement, molette scrolle, ↑ rappelle,
Ctrl+↑ saute, curseur ninja visible au streaming).
