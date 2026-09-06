# Task 6 — Mission-control : vue plein écran (S4)

Plan : `docs/superpowers/plans/2026-09-05-p3-vision-web-workflows-mission-control.md`
Spec (autorité) : `docs/superpowers/specs/2026-09-05-p3-vision-web-workflows-mission-control-design.md` § S4

## Livrable

`crates/kaji-cli/src/tui/missioncontrol.rs` (nouveau, ~880 lignes tests compris) — la vue seule.
Les interactions riches (Enter fiche, `x`, `p`, `s`, `g`) restent à T7 ; la sélection est déjà
posée et rendue, prête à les recevoir.

## Touche d'ouverture

**`f` depuis le volet forge**, plus `/forge full`. `q` (et `Esc`) referment et rendent la main
au volet.

- `f` s'ajoute à `forge_key` (à côté de j/k, ⏎, x, Esc) — aucune collision : le volet
  n'écoutait pas `f`.
- `/forge` sans argument garde le basculement du volet ; `forge_command_arg` (patron
  `edit_command_arg`) route `full`, et tout autre argument rend l'usage.
- Le pied du volet advertise la touche : `FORGE_FOOTER` passe de
  `" ↑/↓ · Enter fiche · x annule "` à `" ⏎ fiche · x coupe · f plein "`, avec un test
  qui garde sa largeur ≤ 30 cellules (le pied est un titre de bloc, ratatui le rogne en
  silence — la touche aurait disparu sans bruit).

## Layout final

```
┌ 炉 mission-control · <workflow> · ‹2 3› ──────────────────────────┐
│collecte                            revue                         │  nom du stage
│en cours                            gate                          │  état du stage
│▋  遣 scan-repo                      門  遣 relecteur                │  carte, ligne 1
│   思 en cours                          gate                       │  carte, ligne 2
│   炭 12k↑ 4.5k↓ · $0.42 · 1m35s        炭 — · 12s                  │  carte, ligne 3
│                                                                  │
│✓  遣 監査エージェント                                                  │
│   terminé                                                        │
│   炭 — · 41s                                                      │
│ …                                                                │
│刻 timeline                                                        │
│scan-repo            ████████████████████████████████████ 1m35s   │
│監査エージェント          ███████████████░░░░░░░░░░░░░░░░░░░░░ 41s     │
└ h/l stages · j/k cartes · q retour ──────────────────────────────┘
```

- **Colonnes** = stages du `WorkflowState` au snapshot ; sans workflow, une seule colonne
  `libre` alimentée par `ForgeState::ordered()` (summons hors workflow).
- **Marques** (champ de 2 cellules, complété à droite — `門` en vaut deux, `✓` une) :
  lame `blade_frame` sur l'horloge de la TÂCHE pour `Running`, `✓` `Done`, `✗`
  `Failed`/`Cancelled`, `⏸` `Paused`, `門` pour un agent `Pending` sous un stage `Waiting`.
  Le libellé de la ligne 2 suit la marque (`gate`, pas « en attente »).
- **Ligne 2** : `火 <outil>` quand un outil brûle, `思 <état>` quand la lame réfléchit, l'état
  nu sinon — 思 ne ment pas sur une carte terminée.
- **Ligne 3** : `炭 in↑ out↓ · $coût · durée` depuis l'usage ledger ; `炭 — · durée` quand le
  ledger n'a pas de ligne. Jamais un zéro : rien ne distinguerait un agent muet d'un agent
  gratuit.
- **Timeline** : bandeau de pied, une barre par agent proportionnelle (arrondi au plus
  proche) à la durée de la plus longue, `█`/`░`, palette du thème actif (accent pour ce qui
  brûle, texte pour un verdict). 5 barres max + ligne `… +N` ; la ligne de reste se prélève
  sur les barres, donc `timeline_lines` consomme exactement la hauteur que `timeline_rows`
  réserve (test dédié). Bandeau supprimé sous 14 lignes : les cartes passent avant.

### Dégradation en largeur

**Scroll horizontal par stage** (le plus simple lisible) : `visible_columns(width, n)` =
`(width + 2) / (34 + 2)`, plancher 1 ; `first_column(selected, visible, n)` fait glisser la
fenêtre sur la sélection, patron du volet forge. Les stages hors champ sont annoncés au titre
(`· ‹2 3›`) — sans quoi une vue étroite ferait croire que le workflow n'a que ce qu'on voit.
La largeur d'une colonne est plafonnée à 36 cellules : sur 200 colonnes, trois stages étirés
à 66 cellules éparpillaient des cartes qui en tiennent 34. Test : à 80/100/120/200 colonnes,
chaque ligne rendue fait exactement `width` — jamais de débordement.

## Les 5 fixes chars-vs-cellules

Primitive partagée ajoutée : `gitstatus::truncate_cells(text, budget)` — pendant tête de
`truncate_left`, `…` compris dans le budget, chaîne vide si le budget ne tient même pas la
marque. `ui::forge_description` a été replié dessus (duplication supprimée).

| Site | Avant | Après | Test |
|---|---|---|---|
| `ui::goal_badge` | `chars().count() > 28` | `truncate_cells(.., 28)` | `the_goal_badge_bounds_the_condition_in_cells_not_chars` (40 kanji) |
| `statusbar::truncate_tool_name` | `chars().count() <= 24` | `truncate_cells(name, 24)` | `the_fire_cuts_a_tool_name_on_cells_not_chars` (24 × 工) |
| `ui::truncate_for_modal` | pré-coupe `chars().take(w*h)` | `prefix_within_cells(text, w*h)` | `the_modal_prefix_is_bounded_in_cells` + `a_clipped_modal_body_fits_its_rows_whatever_le_script` |
| `app::forge_sheet_title` | `chars().take(60)` (sans marque) | `truncate_cells(.., 60)` | `a_sheet_title_is_bounded_in_cells_not_chars` (80 kanji) |
| `app::wrap_words` | `chars().count() + 1 + …` | `display_width`, largeur courante suivie | `wrapping_counts_cells_not_chars` + `wrapping_leaves_an_oversized_word_on_its_own_line` |

Notes :
- `truncate_for_modal` mentait **dans les deux sens** : `w*h` chars gardait deux fois trop de
  CJK à parcourir, et coupait un texte dense en marques de largeur nulle avant que le
  `Paragraph` ait eu à le faire (le modal annonçait alors une coupe qui n'avait pas lieu).
  `prefix_within_cells` borne en cellules, avec un plafond dur `8 chars/cellule` pour garder
  le parcours linéaire face à une chaîne hostile.
- `forge_sheet_title` gagne au passage le `…` qui manquait : la coupe se dit maintenant.
- `wrap_words` suit sa largeur courante au lieu de la recalculer : le repli reste linéaire.

## Données

- **Workflow** : `App::apply_workflow_snapshot(Option<WorkflowState>)` — seam prête, borne la
  sélection à chaque appel (un workflow qui rétrécit ne laisse pas la sélection désigner un
  stage disparu). **Non câblée dans la boucle TUI** : la session TUI ne pilote aucun workflow
  aujourd'hui (`kaji workflow run` tourne dans sa propre session CLI). Voir écarts.
- **Summons libres** : `ForgeState` existant (`subagent_snapshot` + notifications MCP), inchangé.
- **Usage ledger** : `App::apply_agent_usage(HashMap<session_id, AgentUsage>)`, alimentée par
  `tui::mod::agent_usage` = `metrics::report(Last7d, Session)`. Fenêtre glissante 7 j plutôt
  que le mois calendaire — une lame lancée hier soir ne perd pas ses lignes à minuit. La
  requête n'est faite **que quand le mission-control est ouvert** (garde `app.mission.open`
  sur le `forge_tick` 1 s) : c'est la seule vue qui lit l'usage par agent.
- Zéro nouvelle dépendance.

## Précédence des touches

`handle_key` : Ctrl+C → modales (finder, pickers) → **mission-control** → accords de volets
(Ctrl+E/F/O) → volets → composer. La vue plein écran prend la touche avant `Ctrl+F` (qui
replierait une forge qu'on ne verrait plus) et avant le composer (une lettre égarée n'y
atterrit jamais), mais **jamais avant une modale** : une approbation d'outil reste lisible et
répondable depuis la vue — `ui::draw` extrait `draw_overlays` et l'appelle sur les deux
chemins. Test : `the_mission_control_swallows_the_pane_chords_and_the_composer`.

## Comptes

| | |
|---|---|
| Fichiers | 1 nouveau (`missioncontrol.rs`), 5 modifiés (`app.rs`, `ui.rs`, `mod.rs`, `gitstatus.rs`, `statusbar.rs`) |
| Tests ajoutés | 35 (17 missioncontrol, 9 app, 4 ui, 4 gitstatus, 1 statusbar) |
| `cargo test -p kaji-cli` | 1008 passed, 0 failed, 1 ignored (+ doctest 1) |
| `cargo clippy -p kaji-cli --all-targets -- -D warnings` | clean |
| `rustfmt --edition 2021` | clean sur les 6 fichiers |
| Échecs préexistants | aucun nouveau (kaji-cli était déjà vert) |

Parité legacy/SM : sans objet — la task est purement TUI, aucun site de boucle agent touché.

## Écarts et consignes T7

1. **`apply_workflow_snapshot` n'est appelée nulle part dans la boucle TUI.** La seam existe et
   est testée, mais rien ne la nourrit : la session TUI ne pilote pas de workflow aujourd'hui.
   T7 (ou T8) doit décider du câblage — soit un `WorkflowHandle` détenu par l'`event_loop`
   quand la session lance un workflow, soit une lecture de `registry::find_workflow_run` sur
   la session courante. **En attendant, le mission-control rend toujours la colonne « libre ».**
2. **Les summons libres n'ont pas de `session_id`.** `SubagentTaskSnapshot` (`crates/kaji/src/agents/mcp_client.rs`)
   porte `id`, pas de session — la carte d'une lame libre affiche donc `炭 —` en pratique. La
   jointure au ledger est déjà écrite (`usage.get(&task.id)`), il suffira que le summon expose
   la session de l'agent. Changement de `kaji` core, hors scope T6.
3. **T7 : les touches à câbler** sont `Enter` (fiche → lecteur, réutiliser `forge_sheet`),
   `x` (`WorkflowHandle::cancel_agent(stage, agent)` ou `Agent::cancel_subagent` en libre),
   `p` (`pause`/`resume` selon `StageState::Paused`), `s` (composer ciblé), `g`
   (`approve`/`deny` → `GateVerdict`). Le rappel de T4 : **`approve()` rend `false` en rejeu**
   — la vue doit lire le verdict, pas supposer que la porte s'est ouverte.
4. **Matrice de précédence à vérifier sous overlays (T7)** : la vue est déjà sous les modales
   et les pickers, mais `s` (composer ciblé) va rouvrir la question — un composer ouvert
   depuis la vue doit rendre `q` au texte, pas à la fermeture.
5. **Le plateau ne fusionne pas workflow et summons libres.** Spec lue en alternative
   (« stages du workflow actif **ou** libre »). Si T7 veut les deux, ajouter une colonne
   `libre` après les stages en filtrant les lames déjà rattachées à un agent de workflow —
   sinon les agents du workflow, qui tournent aussi comme subagents, compteraient deux fois.
