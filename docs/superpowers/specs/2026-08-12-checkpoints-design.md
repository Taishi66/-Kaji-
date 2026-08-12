# Spec — Checkpoints (snapshot/restore par tour, couplé conversation)

Statut : approuvée en design (forks tranchés session 2026-08-12 : restore = fichiers **+** conversation couplés ; MVP = snapshot/list/restore ; **+ 3 amendements imposés par le premortem** `09 - Meta/premortems/kaji-checkpoints-2026-08-12.md` : undo-the-undo au MVP, frontière par `message_id`, mutex du store). Inspiration : composant checkpoints Hermes-Agent (store git bare, `commit-tree`→`refs/kaji/<hash>`, « undo the undo »).
Scope : nouveau module store (crate `kaji` ou `kaji-core`) + hook `Agent::reply()` + surface TUI (`/checkpoints`, `/restore`). Réutilise le substrat event log (`session_events`).

## Objectif

Rendre chaque tour **annulable** : snapshot automatique de l'arbre de travail avant chaque tour, restore manuel (jamais automatique) qui remet fichiers **et** conversation à l'état d'un tour choisi. Filet contre un tour qui casse le code ou part en vrille.

## Limite de provenance (constatée T1, mitigée — pas un non-objectif)

La couche store **ne trace pas la provenance** : `files_created_since` (reverse-diff `--diff-filter=A`) ne distingue pas un ajout par tool-call d'un fichier créé à la main par l'utilisateur dans la fenêtre snapshot→restore — les deux sont des chemins nouveaux absents du tree cible. Deux mitigations rendent ça acceptable au MVP : (1) `add -A` respecte le `.gitignore` → `.env`/`node_modules`/artefacts (quasi toujours gitignorés) ne sont jamais capturés ni supprimés ; (2) le snapshot **pre-restore** (undo-the-undo, au MVP) capture tout non-gitignoré avant la mutation → un fichier utilisateur supprimé à tort est récupérable par `/restore <pre-restore>`. Le résidu non couvert : un fichier non-gitignoré créé à la main entre snapshot et restore est supprimé par le restore (mais récupérable via pre-restore). Provenance réelle (via les events tool du journal) = v2. T5 (review/E2E) doit tester le résidu ET sa récupération.

## Non-objectifs (v2+)

- Preuve de volume `dev+ino` (anti-purge sur dir déplacé/remonté). MVP : restore sur le même chemin qu'au snapshot, best-effort.
- `git gc`/rotation/rétention du store (grandit avec la session ; dédup blob limite la casse).
- Branches/arborescence de checkpoints (redo multi-chemins). MVP : undo-the-undo = un seul snapshot pre-restore, pas un arbre.
- Skip-si-arbre-inchangé (optimisation du scan `add -A`).

## Architecture — 3 unités

### 1. `CheckpointStore` (store git bare par projet)

- **Emplacement** : `Paths::in_data_dir("kaji/checkpoints").join(<project_key>.git)`, dépôt **bare**, séparé du git de l'utilisateur.
- **Identité projet** (`project_key`) : si le projet est un repo git → hash du chemin du toplevel `.git` (`git rev-parse --show-toplevel`) ; sinon → hash du `current_dir` canonicalisé. **La fonction est figée** (test de non-régression sur des chemins connus) — la changer orphelinerait les stores existants (premortem PM7). Doc-comment : « ne jamais modifier sans migration ».
- **Sérialisation** : un `Mutex` **dans le store** enveloppe toute opération git (index unique du bare — snapshot et restore concurrents corrompent `index.lock`, premortem PM6). Ne pas hériter la sérialisation de la boucle TUI mono-thread.
- **API** (toutes via `subprocess::git_command()`, `--git-dir=<store> --work-tree=<projet>`) :
  - `snapshot(&self, project: &Path, label: &str) -> Result<CheckpointId>` : `add -A` → `write-tree` → `commit-tree` (parent = HEAD du ref précédent si présent) → écrit `refs/kaji/<sha256[12]>`. Retourne `{ id, tree_sha }`.
  - `files_created_since(&self, project: &Path, target_tree: &str) -> Result<Vec<PathBuf>>` : `git diff --name-only --diff-filter=A <target_tree> <arbre courant>`. **Fonction nommée et testée isolément** — sa suppression doit être un acte visible (premortem PM5).
  - `restore(&self, project: &Path, target: &CheckpointId) -> Result<()>` : (a) `read-tree <target>` + `checkout-index -f -a` (restaure les fichiers du snapshot) ; (b) `rm` **uniquement** les chemins de `files_created_since(target)`. **JAMAIS `git clean`** (effacerait les non-suivis légitimes de l'utilisateur — `.env`, `node_modules` ; premortem PM5). Doc-comment gras là-dessus.

### 2. Déclencheur — snapshot au `turn_start`

- Hook dans l'enveloppe `async_stream` de `Agent::reply()`, au même point que l'event log `turn_start`, capturant l'état **AVANT** l'exécution du tour. Sémantique : `checkpoint(turn_seq=N)` = état d'avant le tour N → `restore(N)` = annuler le tour N. Payload de l'event `checkpoint` : `{ checkpoint_id, tree_sha, captured: "pre_turn", boundary_message_id }` où `boundary_message_id` = le **dernier `message_id` persisté** au moment du snapshot (pas un timestamp — immunise le mapping contre la compaction, premortem PM3). `boundary_message_id` peut être `null` (aucun message encore) → restore couplé refusé proprement.
- **Non-fatal et non-bloquant** : réutiliser mot pour mot le pattern `resolve_turn_seq` du fix event-log — un helper qui prend `Result<CheckpointId>` → `Option`, `warn!` sur Err, le tour continue. **JAMAIS `?`** (premortem PM4 = répétition du Major `next_turn_seq`). Doc-comment pointant vers ce post-mortem.
- Persistance : un event `checkpoint` dans `session_events` (kind existant à ajouter à l'énumération conceptuelle — pas de schéma nouveau, `payload_json` porte tout).

### 3. Surface TUI (`/checkpoints`, `/restore`)

- Deux entrées dans la table `COMMANDS` (app.rs).
- `/checkpoints` : liste les events `checkpoint` de la session/projet — `tour N · HH:MM · <début du prompt sanitizé>`. Le prompt-preview passe par `sanitize_for_display` (comme la ligne « tour interrompu »).
- `/restore <id>` : **modal y/n obligatoire** (destructif — réutilise le pattern gate/tool-approval). « jamais automatique » = toujours confirmé.
- **Gardé** : `/restore` refusé si `turn_active || turn_pending` (l'index du store et l'arbre ne doivent pas bouger sous un tour actif ; premortem PM6). Message : « termine ou annule le tour avant de restaurer ».

## Restore couplé — séquence atomique (le cœur de la sûreté)

`/restore <N>` confirmé exécute, **dans cet ordre, tout-ou-rien** :

1. **Snapshot pre-restore** (undo-the-undo, avancé au MVP) : `store.snapshot(project, "pre-restore")` AVANT toute mutation → rend PM2/PM5 récupérables. Event `checkpoint` avec `label: "pre-restore"`.
2. **Git restore** : `store.restore(project, N)`. Si échec → **abandon**, aucune mutation de conversation, message d'erreur. Rien n'est à moitié fait.
3. **Truncate conversation** : `SessionManager::truncate_conversation_from_message(session, boundary_message_id_de_N)`. Si `boundary_message_id` est `null` → refus explicite du restore couplé avec message clair (ne pas tronquer au hasard). **Échec ici = FATAL et bruyant** (`?` + context) : git a déjà écrasé les fichiers, un truncate non fait = état incohérent → le message système invite à re-restorer. **Ne JAMAIS rendre ce truncate non-fatal** (premortem PM2) — doc-comment : « transaction logique avec l'étape 2 ».
4. Message système : « ⚠ restauré au tour N — arbre et conversation alignés ; `/restore` du snapshot pre-restore pour annuler ».

## Vérification

Tests store (dépôt temp réel) :
- snapshot → modifie 3 fichiers → restore → arbre revient au tree exact.
- **`restore_preserves_untracked_files_outside_the_snapshot`** (barrière PM5) : `secret.env` non-suivi créé hors snapshot → survit au restore. Échoue sous `git clean`.
- séquence 3 tours (fichiers `a`/`b`/`c`) → `restore` tour 2 → `a` présent, `b`/`c` absents (barrière PM1, sémantique pre-turn).
- `files_created_since` testé isolément.

Tests intégration :
- **`a_failed_snapshot_does_not_abort_the_turn`** (barrière PM4, calqué sur l'event-log) : snapshot forcé en échec → le tour se termine normalement, pas de message perdu.
- restore couplé : git réussit + truncate forcé en échec → le handler retourne une erreur, ne prétend PAS « restauré » (barrière PM2).
- `boundary_message_id` null → restore couplé refusé proprement.
- compaction entre snapshot et restore → truncate sur `message_id` stable, ou refus si l'id a été effacé (barrière PM3).

Tests TUI : `/checkpoints` liste ; `/restore` ouvre le modal ; `/restore` refusé pendant `turn_active`.

Baseline : kaji --lib 8 préexistants inchangés ; kaji-cli vert ; clippy scoped ; fmt ; migration DB inutile (réutilise `session_events`).

## Question ouverte (au plan)

Mapping `turn_seq → boundary_message_id` : le snapshot stocke le `boundary_message_id` au moment du `turn_start`. Vérifier au plan que `last_message_id` est atteignable depuis `Agent::reply()` (via `session_manager`) à ce point — sinon, le calculer côté SessionManager au moment de l'append de l'event checkpoint.
