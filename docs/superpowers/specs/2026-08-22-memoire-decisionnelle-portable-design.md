# Mémoire décisionnelle portable — Design (P1)

Date : 2026-08-22 · Statut : validé (brainstorm S1-S6, go user) · Prochaine étape : plan d'implémentation

## Contexte et objectif

La mémoire actuelle de kaji est un journal brut : `ingest_turn` (`crates/kaji/src/kaji.rs`) stocke
les 3 derniers messages user verbatim dans SQLite (`shared.db`, moteur FTS5 dans
`crates/kaji-core/src/memory.rs`), et le recall re-splice ce texte brut dans le prompt. Résultat :
du bruit verbatim, aucune curation, aucune portabilité (la mémoire est enfermée dans une base
SQLite locale, invisible pour git, un collègue, ou un autre harness).

Objectif : une mémoire **décisionnelle** (faits curés : décisions, gotchas, préférences,
références) et **portable** (fichiers markdown lisibles, diffables, committables — le repo git
est le canal qui voyage entre machines et entre utilisateurs).

## Décisions verrouillées

| Décision | Justification |
|----------|---------------|
| Source de vérité = fichiers md ; index FTS dérivé, rebuildable | Lisible/éditable hors-kaji (autre harness, collègue, qmd) ; l'index se reconstruit, jamais l'inverse |
| Extraction hybride : heuristique 0-token à chaud + curateur LLM async | Crash-safe et gratuit en hot-path ; la qualité vient de la curation hors latence |
| Double scope dès v1 : projet in-repo + user en data dir | Le git est le seul canal qui voyage ; les préférences perso ne vont jamais dans un repo partagé |
| 1 fait = 1 fichier typé + `MEMORY.md` index généré | Dédup/péremption par fichier, diffs git propres, format déjà bien lu par les autres harness |
| Architecture A : journal brut SQLite intact + promotion curée en md | Zéro régression (parité legacy/state-machine conservée), migration incrémentale |
| Retrait de l'extension MCP memory héritée (`crates/kaji-mcp/src/memory/`) + migration one-shot de ses JSON | Elle écrit du JSON dans `.kaji/memory` (collision de chemin) ; un seul système mémoire dans le produit |

## S1 — Modèle de données

**Scopes.**
- **Projet** : `<racine git>/.kaji/memory/`. Hors repo git : fallback data dir
  `kaji/memory/projects/<slug-du-chemin>/` (même stratégie de data dir que `shared.db`,
  cf. `Paths::in_data_dir("kaji/memory")`, `kaji.rs:43`).
- **User** : data dir `kaji/memory/user/`.

**Fichier fait** : `<type>-<slug>.md`, type ∈ {`decision`, `gotcha`, `preference`, `reference`}.
Slug strictement `[a-z0-9-]+` (guard path-traversal, voir S2). Frontmatter :

```markdown
---
type: decision
description: <résumé une ligne, utilisé par MEMORY.md et le recall>
date: 2026-08-22
session: <session-id>
created_by: curator | user
---

<corps du fait>
```

**Routage mécanique** : `preference` → scope user ; `decision`/`gotcha`/`reference` → scope
projet. Pas d'heuristique LLM sur le routage — le type décide.

**`MEMORY.md`** : index généré (une ligne par fait : lien + description), régénéré à chaque
écriture. Jamais édité à la main ; un edit manuel est écrasé sans merge.

**Journal brut** : `shared.db` inchangé dans son rôle, plus une colonne `curated_at INTEGER NULL`
sur `memory_entries` — migration additive sur le pattern de `migrate_session_id`
(`memory.rs` : `PRAGMA table_info` + `ALTER TABLE ADD COLUMN`, metadata-only). `curated_at IS
NULL` = entrée pas encore vue par le curateur.

## S2 — Curateur LLM async

**Triggers** (3) :
1. Fin de tour, si ≥ 5 entrées brutes non curées, avec debounce (pas plus d'un run par 5 min)
   — placé dans le bridge partagé (`kaji.rs`, après `ingest_turn`) pour que les
   deux boucles agent (legacy et state-machine) l'héritent sans double implémentation.
2. `/remember [note]` (voir S5) — écriture **immédiate et 0-token** d'un fait
   `created_by: user` (type et défauts en S5), puis un run de curation part sur le journal en
   attente.
3. `kaji memory curate` (CLI, run explicite).

**Modèle** : `fast_model` (`crates/kaji/src/model_config.rs`) si configuré, sinon le modèle de
session ; override par env `KAJI_MEMORY_CURATOR_MODEL`.

**Contrat d'appel** : 1 appel LLM par run. Entrée = entrées brutes non curées + index des faits
existants (slugs + descriptions). Sortie = JSON strict, liste d'opérations
`{action: create|update, type, slug, description, body}` — le scope découle du type (S1).
**Cap : 5 opérations par run** ; au-delà, tronqué. **Fail-closed** : JSON invalide, erreur
API, timeout → aucune écriture, `curated_at` non tamponné, le lot repart au prochain trigger.
Succès → tampon `curated_at` sur les entrées consommées.

**Sécurité** :
- `redact_text` (`crates/kaji/src/session/redact.rs`) appliqué à tout contenu écrit en scope
  projet — le repo est un canal partagé, aucun secret n'y descend.
- Slug validé `[a-z0-9-]+` avant toute construction de chemin (path-traversal).
- Un fait `created_by: user` n'est **jamais** modifié par le curateur (une op `update` visant
  un tel fichier est ignorée et loggée).

## S3 — Recall

**Index** : `index.db` (FTS5, même moteur que `memory.rs`) en **data dir uniquement** — jamais
dans le repo, jamais committé. Un index par scope, rebuild incrémental par comparaison mtime
(fichiers md vs index) au boot et après chaque run de curation. Suppression d'un fichier md →
l'entrée sort de l'index au rebuild suivant.

**Splice inchangé** : les deux sites de splice (`agent.rs:993-1022`,
`ops_llm.rs:397-408`) ne bougent pas — seule la *composition* du bloc mémoire change dans le
bridge (`recall_prompt` / `splice_memory_block`, `kaji.rs`). Parité legacy/state-machine par
construction.

**Composition du bloc** : (1) faits curés top-k = 3 (même requête que le recall actuel,
construite par `recall_prompt`, scopes user + projet confondus, classement bm25), puis
(2) journal brut récent comme aujourd'hui. Budget token AIAD existant intact — les faits curés consomment le même
budget, en tête.

## S4 — Migration et cohabitation

- **Extension MCP memory** (`crates/kaji-mcp/src/memory/`) : retirée. Au premier boot qui
  détecte ses JSON dans `.kaji/memory` : migration one-shot JSON → fichiers md (type
  `reference` par défaut, `created_by: curator`), originaux renommés `*.legacy`, jamais
  supprimés.
- **`shared.db` existant** : rien de spécial — les entrées historiques ont `curated_at NULL`
  et sont absorbées au fil de l'eau par les runs de curation normaux.
- **Git** : committer `.kaji/memory/` est le choix du user. kaji ne touche jamais au
  `.gitignore` et n'exécute jamais de commande git sur ce dossier.

## S5 — CLI / UX

| Surface | Comportement |
|---------|--------------|
| `/remember [note]` (in-session) | Écriture immédiate d'un fait `created_by: user`. Type par mot-clé optionnel en tête de note (`decision:`, `gotcha:`, `preference:`, `reference:`), défaut `preference` — scope user : rien ne descend dans le repo sans intention explicite. Puis run de curation sur le journal en attente |
| `kaji memory list --curated \| --raw` | Liste les faits md (par scope) ou le journal brut |
| `kaji memory curate` | Run de curation explicite |
| Suppression d'un fait | Supprimer le fichier md — pas de commande dédiée ; l'index suit au rebuild |
| Ligne système TUI | `記 N faits mémorisés` après un run de curation qui a écrit N faits |

CLI existante (`cli.rs:757-792`, `commands/memory.rs`) : les sous-commandes actuelles restent,
`list` gagne les deux flags.

## S6 — Erreurs et tests

**Robustesse** :
- Toute écriture md + `MEMORY.md` : atomique via `NamedTempFile::new_in(dir)` + persist —
  pattern existant de `config/permission.rs:197`.
- Le curateur tourne en async post-tour : son échec ne bloque, ne ralentit et ne modifie
  jamais le tour en cours. Fail-closed partout (S2).
- Fichier md corrompu (frontmatter invalide) : ignoré au rebuild d'index, warning loggé, jamais
  supprimé.

**Plan de test** :
1. Index : rebuild mtime (création/modif/suppression de fichiers md), corruption frontmatter.
2. Curateur : routage par type, redaction en scope projet, dédup par slug (update vs create),
   cap 5 ops, fail-closed (JSON invalide → zéro écriture, `curated_at` intact), immutabilité
   `created_by: user`.
3. Parité : les deux chemins agent (legacy / `KAJI_STATE_MACHINE=1`) produisent le même bloc
   mémoire sur la même entrée.
4. Migration MCP : JSON → md, originaux `.legacy`, idempotence (2e boot = no-op).
5. E2E 2 sessions : fait curé en session 1 → recall en session 2 (via `kaji-self-test.yaml`
   mis à jour, cf. règle AGENTS.md).

## Non-goals

- Pas d'embeddings/vecteurs — FTS5 seul.
- Pas de partage mémoire inter-agents au-delà des deux scopes.
- Pas d'UI TUI de navigation des faits (la ligne système suffit en v1).
- Pas d'auto-commit ni de gestion `.gitignore` par kaji.
- Pas de sync réseau — le canal de portabilité est git, point.
