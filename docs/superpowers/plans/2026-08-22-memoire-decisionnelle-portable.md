# Mémoire décisionnelle portable — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ajouter à kaji une mémoire de faits curés en fichiers markdown (2 scopes : projet in-repo `.kaji/memory/`, user en data dir), promue depuis le journal brut SQLite par un curateur LLM async, retrouvée via un index FTS5 dérivé, sans toucher aux deux sites de splice existants.

**Architecture:** Le journal brut (`shared.db`) reste intact et gagne une colonne `curated_at`. Un nouveau module `facts` dans `kaji-core` porte le modèle de fait (md + frontmatter), le store par répertoire et l'index FTS5 rebuildable. Le crate `kaji` orchestre : résolution des scopes, curateur (appel one-shot `complete_fast`, fail-closed), trigger fin-de-tour débouncé (`tokio::spawn`), composition du bloc de recall, `/remember`, CLI. L'extension MCP memory héritée est retirée avec migration one-shot de ses `.txt`.

**Tech Stack:** Rust, rusqlite 0.32 (FTS5), serde_yaml 0.9 (workspace), tempfile (workspace), tokio, clap.

**Spec:** `docs/superpowers/specs/2026-08-22-memoire-decisionnelle-portable-design.md` — la lire en entier avant toute tâche.

## Global Constraints

- `source bin/activate-hermit` avant tout cargo.
- Un seul processus cargo à la fois (verrou `target/`) ; cargo toujours en **foreground**.
- `cargo fmt` **jamais** global : uniquement `cargo fmt -- <fichiers touchés>` (repo mixte éditions 2021/2024).
- Erreurs : `anyhow::Result` (règle AGENTS.md) ; pas de `.context()` qui ne dit rien de plus.
- Zéro commentaire qui paraphrase le code ; commentaires seulement pour du non-évident.
- Parité agent-loop : toute modif de comportement de tour doit exister dans les 2 chemins (legacy `agents/agent.rs` et `agents/state_machine/`) — ici obtenue en ne touchant que le bridge partagé `crate::kaji` + 1 ligne identique par site.
- Fail-closed partout dans le curateur : la moindre erreur ⇒ aucune écriture, `curated_at` non tamponné.
- Écritures fichier atomiques : `NamedTempFile::new_in(dir)` + `persist` (pattern `config/permission.rs:192-202`).
- Slugs : `[a-z0-9-]+`, 1-64 chars — validés avant toute construction de chemin.
- kaji ne touche jamais `.gitignore` et n'exécute jamais git.
- Constantes verrouillées par la spec : cap **5** ops/run curateur, recall faits top-k **3**, trigger ≥ **5** entrées non curées, debounce **300 s**.
- Tests qui touchent l'env (`KAJI_PATH_ROOT`, `KAJI_MEMORY_DIR`, `KAJI_STATE_MACHINE`) : toujours via `env_lock::lock_env` (pattern `crates/kaji/tests/kaji_memory_test.rs:182-207`).
- Commits fréquents, messages conventionnels français (`feat(memory): …`), un commit par tâche minimum.
- Les chemins d'import dans les extraits (`kaji_core::facts`, `kaji::kaji::…`) sont indicatifs : vérifier le vrai nom de lib/module au premier échec de compilation (regarder comment les tests existants du même crate importent).

---

### Task 1: kaji-core — colonne `curated_at` + API `uncurated`/`mark_curated`

**Files:**
- Modify: `crates/kaji-core/src/memory.rs`, `crates/kaji-core/Cargo.toml` (ajouter `tempfile = { workspace = true }` aux `[dev-dependencies]` s'il n'y est pas)
- Test: `crates/kaji-core/src/memory.rs` (module `#[cfg(test)]` existant — suivre le style interne du fichier)

**Interfaces:**
- Consumes: schéma existant `memory_entries` (`memory.rs:133-162`), pattern `migrate_session_id` (`memory.rs:57-69`), `Memory::open` (`memory.rs:187`), `now_secs()` (`memory.rs:41`).
- Produces: `pub fn uncurated(&self, limit: usize) -> Vec<Entry>` ; `pub fn mark_curated(&mut self, ids: &[u64])`. `curated_at INTEGER NULL` en **secondes** epoch. `Entry` inchangé.

- [ ] **Step 1: Écrire les tests qui échouent** (dans le module de test existant de `memory.rs`)

```rust
#[test]
fn uncurated_returns_unstamped_oldest_first_and_respects_limit() {
    let mut mem = Memory::new();
    let a = mem.remember("fact a", &[], None);
    let b = mem.remember("fact b", &[], None);
    let c = mem.remember("fact c", &[], None);
    mem.mark_curated(&[b]);
    let pending = mem.uncurated(10);
    assert_eq!(pending.iter().map(|e| e.id).collect::<Vec<_>>(), vec![a, c]);
    assert_eq!(mem.uncurated(1).len(), 1);
}

#[test]
fn mark_curated_is_idempotent_and_ignores_unknown_ids() {
    let mut mem = Memory::new();
    let a = mem.remember("fact a", &[], None);
    mem.mark_curated(&[a, 9999]);
    mem.mark_curated(&[a]);
    assert!(mem.uncurated(10).is_empty());
}

#[test]
fn open_migrates_curated_at_on_existing_db() {
    // Base créée sans la colonne : simuler en ouvrant, droppant la colonne étant impossible
    // en SQLite ancien — à la place, vérifier que open() sur une base v-courante expose l'API
    // et que PRAGMA table_info contient curated_at (le pattern migrate_session_id est déjà
    // prouvé par les tests existants).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("m.db");
    let mem = Memory::open(&path).unwrap();
    drop(mem);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let mut stmt = conn.prepare("PRAGMA table_info(memory_entries)").unwrap();
    let cols: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(cols.iter().any(|c| c == "curated_at"));
}
```

- [ ] **Step 2: Vérifier l'échec**

Run: `cargo test -p kaji-core uncurated` — attendu : erreur de compilation « no method named `uncurated` ».

- [ ] **Step 3: Implémenter**

Dans `memory.rs`, à côté de `migrate_session_id` (même pattern exact, `ALTER TABLE` metadata-only — FTS5 external-content non affecté car la colonne n'est pas indexée) :

```rust
fn migrate_curated_at(conn: &Connection) -> rusqlite::Result<()> {
    let mut columns = conn.prepare("PRAGMA table_info(memory_entries)")?;
    let names = columns
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !names.iter().any(|name| name == "curated_at") {
        conn.execute_batch("ALTER TABLE memory_entries ADD COLUMN curated_at INTEGER")?;
    }
    Ok(())
}
```

- Appeler `migrate_curated_at(&conn)?` dans `Memory::open` juste après `migrate_session_id` (`memory.rs:192`). Ajouter la colonne `curated_at INTEGER` au `SCHEMA` (`CREATE TABLE IF NOT EXISTS` ne migre pas l'existant — les deux sont nécessaires, comme pour `session_id`).
- API (suivre le style des méthodes existantes, requêtes préparées) :

```rust
pub fn uncurated(&self, limit: usize) -> Vec<Entry> {
    // SELECT id, text, entities, body, ts, ttl_ms, session_id FROM memory_entries
    //  WHERE curated_at IS NULL ORDER BY ts ASC, id ASC LIMIT ?1
    // — réutiliser le mapping row->Entry existant (voir iter()/get()).
}

pub fn mark_curated(&mut self, ids: &[u64]) {
    // UPDATE memory_entries SET curated_at = ?1 WHERE id = ?2, dans une transaction,
    // ?1 = now_secs()
}
```

- [ ] **Step 4: Vérifier le vert** — `cargo test -p kaji-core` : tous verts, y compris les tests existants (non-régression migration).
- [ ] **Step 5: Format + commit**

```bash
cargo fmt -- crates/kaji-core/src/memory.rs
git add crates/kaji-core/src/memory.rs
git commit -m "feat(memory): colonne curated_at + API uncurated/mark_curated sur le journal brut"
```

---

### Task 2: kaji-core — modèle `Fact` (frontmatter, slug, rendu md)

**Files:**
- Create: `crates/kaji-core/src/facts/mod.rs`
- Modify: `crates/kaji-core/src/lib.rs` (déclarer `pub mod facts;`), `crates/kaji-core/Cargo.toml` (ajouter `serde = { workspace = true, features = ["derive"] }`, `serde_yaml = { workspace = true }`, `tempfile = { workspace = true }`, `anyhow = { workspace = true }` — **vérifier d'abord lesquels y sont déjà** ; s'ils ne sont pas au `[workspace.dependencies]` racine pour `serde`/`anyhow`, reprendre la ligne de version d'un autre crate du workspace)
- Test: `crates/kaji-core/tests/facts_test.rs`

**Interfaces:**
- Consumes: rien du reste du code.
- Produces:

```rust
pub enum FactType { Decision, Gotcha, Preference, Reference }
impl FactType {
    pub fn as_str(&self) -> &'static str;          // "decision" | "gotcha" | "preference" | "reference"
    pub fn parse(s: &str) -> Option<FactType>;
}
pub enum CreatedBy { Curator, User }               // as_str/parse idem: "curator" | "user"
pub struct Fact {
    pub fact_type: FactType,
    pub slug: String,
    pub description: String,
    pub date: String,        // YYYY-MM-DD, fourni par l'appelant
    pub session: String,
    pub created_by: CreatedBy,
    pub body: String,
}
impl Fact {
    pub fn file_name(&self) -> String;                                  // "<type>-<slug>.md"
    pub fn to_markdown(&self) -> String;                                // frontmatter YAML + body
    pub fn parse(file_name: &str, content: &str) -> Option<Fact>;       // None si invalide
}
pub fn validate_slug(slug: &str) -> bool;   // ^[a-z0-9-]{1,64}$, pas de '--' exigé, refuse vide
pub fn slugify(text: &str) -> String;       // lowercase, non-[a-z0-9] -> '-', collapse, trim '-', tronqué 64
```

- [ ] **Step 1: Tests qui échouent** (`crates/kaji-core/tests/facts_test.rs`)

```rust
use kaji_core::facts::{slugify, validate_slug, CreatedBy, Fact, FactType};

fn sample() -> Fact {
    Fact {
        fact_type: FactType::Decision,
        slug: "index-fts-derive".into(),
        description: "L'index FTS est dérivé, jamais source de vérité".into(),
        date: "2026-08-22".into(),
        session: "s1".into(),
        created_by: CreatedBy::Curator,
        body: "Décision : les fichiers md sont la source de vérité.".into(),
    }
}

#[test]
fn roundtrip_markdown() {
    let fact = sample();
    let md = fact.to_markdown();
    assert!(md.starts_with("---\n"));
    let parsed = Fact::parse(&fact.file_name(), &md).unwrap();
    assert_eq!(parsed.slug, "index-fts-derive");
    assert_eq!(parsed.fact_type.as_str(), "decision");
    assert_eq!(parsed.body, fact.body);
    assert!(matches!(parsed.created_by, CreatedBy::Curator));
}

#[test]
fn parse_rejects_invalid() {
    assert!(Fact::parse("decision-x.md", "pas de frontmatter").is_none());
    assert!(Fact::parse("decision-x.md", "---\ntype: nope\n---\nbody").is_none());
    assert!(Fact::parse("weird.md", &sample().to_markdown()).is_none()); // nom sans type-slug
}

#[test]
fn slug_rules() {
    assert!(validate_slug("abc-123"));
    assert!(!validate_slug(""));
    assert!(!validate_slug("../evil"));
    assert!(!validate_slug("UPPER"));
    assert!(!validate_slug(&"a".repeat(65)));
    assert_eq!(slugify("Éviter les Chemins ../foo !"), "viter-les-chemins-foo");
}
```

- [ ] **Step 2: Vérifier l'échec** — `cargo test -p kaji-core --test facts_test` : erreur de compilation (module absent).
- [ ] **Step 3: Implémenter** `facts/mod.rs` :

```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct FactMeta {
    #[serde(rename = "type")]
    fact_type: String,
    description: String,
    date: String,
    #[serde(default)]
    session: String,
    created_by: String,
}
```

- `to_markdown()` : `format!("---\n{}---\n\n{}\n", serde_yaml::to_string(&meta)?, body.trim_end())` — le `?` remplacé par `expect` est interdit : `serde_yaml::to_string` sur cette struct ne peut pas échouer, utiliser `.expect("frontmatter serialization")`.
- `parse(file_name, content)` : exige préfixe `---\n`, cherche `\n---\n` de fermeture, `serde_yaml::from_str::<FactMeta>` sur le bloc, `FactType::parse` + `CreatedBy::parse` sur les strings (sinon `None`) ; slug = `file_name.strip_suffix(".md")` puis `strip_prefix("<type>-")` (le type vient du frontmatter et doit matcher le préfixe du nom, sinon `None`) ; `validate_slug` obligatoire ; body = tout après la fermeture, `trim_start()`.
- `slugify` : itérer `char::to_lowercase`, mapper `[a-z0-9]` sinon `'-'`, écraser les répétitions de `'-'`, trim, tronquer à 64. NB : les caractères non-ASCII sont droppés (remplacés par `-` puis collapsés) — c'est le comportement testé (`Éviter` → `viter`).

- [ ] **Step 4: Vert** — `cargo test -p kaji-core --test facts_test`.
- [ ] **Step 5: Format + commit**

```bash
cargo fmt -- crates/kaji-core/src/facts/mod.rs crates/kaji-core/src/lib.rs crates/kaji-core/tests/facts_test.rs
git add -A crates/kaji-core
git commit -m "feat(facts): modèle Fact md+frontmatter, slug validé, roundtrip parse/render"
```

---

### Task 3: kaji-core — `FactStore` (répertoire, écriture atomique, MEMORY.md)

**Files:**
- Create: `crates/kaji-core/src/facts/store.rs` (déclarer `mod store; pub use store::FactStore;` dans `facts/mod.rs`)
- Test: `crates/kaji-core/tests/facts_store_test.rs`

**Interfaces:**
- Consumes: `Fact`, `Fact::parse`, `Fact::to_markdown`, `validate_slug` (Task 2).
- Produces:

```rust
pub struct FactStore { dir: PathBuf }
impl FactStore {
    pub fn new(dir: PathBuf) -> Self;                          // ne crée rien sur disque
    pub fn dir(&self) -> &Path;
    pub fn list(&self) -> Vec<Fact>;                           // *.md sauf MEMORY.md ; invalides ignorés (tracing::warn si dispo, sinon eprintln interdit — utiliser log/tracing du crate ; si kaji-core n'a ni l'un ni l'autre: ignorer silencieusement, le warn vivra côté kaji)
    pub fn get(&self, fact_type: &FactType, slug: &str) -> Option<Fact>;
    pub fn write(&self, fact: &Fact) -> anyhow::Result<()>;    // atomique + régénère MEMORY.md
}
```

- [ ] **Step 1: Tests qui échouent**

```rust
use kaji_core::facts::{CreatedBy, Fact, FactStore, FactType};

fn fact(slug: &str) -> Fact {
    Fact {
        fact_type: FactType::Decision,
        slug: slug.into(),
        description: format!("description de {slug}"),
        date: "2026-08-22".into(),
        session: "s1".into(),
        created_by: CreatedBy::Curator,
        body: format!("corps du fait {slug}"),
    }
}

#[test]
fn write_then_list_and_get() {
    let dir = tempfile::tempdir().unwrap();
    let store = FactStore::new(dir.path().to_path_buf());
    store.write(&fact("premier")).unwrap();
    store.write(&fact("second")).unwrap();
    assert_eq!(store.list().len(), 2);
    assert!(store.get(&FactType::Decision, "premier").is_some());
    assert!(store.get(&FactType::Decision, "absent").is_none());
}

#[test]
fn write_regenerates_memory_index() {
    let dir = tempfile::tempdir().unwrap();
    let store = FactStore::new(dir.path().to_path_buf());
    store.write(&fact("premier")).unwrap();
    let index = std::fs::read_to_string(dir.path().join("MEMORY.md")).unwrap();
    assert!(index.contains("decision-premier.md"));
    assert!(index.contains(&fact("premier").description));
}

#[test]
fn corrupt_file_is_skipped_not_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let store = FactStore::new(dir.path().to_path_buf());
    store.write(&fact("ok")).unwrap();
    std::fs::write(dir.path().join("gotcha-corrompu.md"), "frontmatter cassé").unwrap();
    assert_eq!(store.list().len(), 1);
    assert!(dir.path().join("gotcha-corrompu.md").exists());
}

#[test]
fn write_rejects_invalid_slug() {
    let dir = tempfile::tempdir().unwrap();
    let store = FactStore::new(dir.path().to_path_buf());
    let mut bad = fact("ok");
    bad.slug = "../evil".into();
    assert!(store.write(&bad).is_err());
}
```

- [ ] **Step 2: Vérifier l'échec** — `cargo test -p kaji-core --test facts_store_test`.
- [ ] **Step 3: Implémenter** :
  - `write` : `validate_slug` sinon `anyhow::bail!` ; `create_dir_all(&self.dir)?` ; écriture atomique du fait **et** de `MEMORY.md` via le pattern `permission.rs:192-202` (`NamedTempFile::new_in(&self.dir)` → `write_all` → `persist(target)`), mais avec `?` (anyhow) au lieu de `expect`.
  - `MEMORY.md` généré : en-tête `# Memory Index\n\n> Généré par kaji — ne pas éditer.\n\n` puis une ligne par fait trié par (type, slug) : `- [<file_name>](<file_name>) — <description>`.
  - `list` : `read_dir`, filtrer `.md` ≠ `MEMORY.md`, `Fact::parse` ; `filter_map`.
- [ ] **Step 4: Vert** — `cargo test -p kaji-core --test facts_store_test`.
- [ ] **Step 5: Format + commit** (`cargo fmt -- <fichiers>` ; `git commit -m "feat(facts): FactStore — écriture atomique, MEMORY.md généré, corruption tolérée"`)

---

### Task 4: kaji-core — `FactIndex` (FTS5 dérivé, rebuild par fingerprint mtime)

**Files:**
- Create: `crates/kaji-core/src/facts/index.rs` (déclarer + `pub use` dans `facts/mod.rs`)
- Test: `crates/kaji-core/tests/facts_index_test.rs`

**Interfaces:**
- Consumes: `FactStore::list` (Task 3).
- Produces:

```rust
pub struct FactHit {
    pub scope: String,        // "project" | "user"
    pub file_name: String,
    pub description: String,
    pub body: String,
}
pub struct FactIndex { conn: rusqlite::Connection }
impl FactIndex {
    pub fn open(db_path: &Path) -> rusqlite::Result<FactIndex>;   // create_dir_all(parent)
    pub fn rebuild_if_stale(&mut self, stores: &[(&str, &FactStore)]) -> anyhow::Result<()>;
    pub fn search(&self, query: &str, k: usize) -> Vec<FactHit>;
}
```

- [ ] **Step 1: Tests qui échouent**

```rust
use kaji_core::facts::{FactIndex, FactStore /* + Fact helpers */};

#[test]
fn rebuild_indexes_and_search_ranks_by_match() {
    let dir = tempfile::tempdir().unwrap();
    let store = FactStore::new(dir.path().join("facts"));
    store.write(&fact_with("cache-ttl", "Le TTL du cache est une heure")).unwrap();
    store.write(&fact_with("autre", "Sans rapport")).unwrap();
    let mut index = FactIndex::open(&dir.path().join("index.db")).unwrap();
    index.rebuild_if_stale(&[("project", &store)]).unwrap();
    let hits = index.search("cache TTL", 3);
    assert_eq!(hits[0].file_name, "decision-cache-ttl.md");
    assert_eq!(hits[0].scope, "project");
}

#[test]
fn rebuild_noops_when_fresh_and_reindexes_on_change() {
    let dir = tempfile::tempdir().unwrap();
    let store = FactStore::new(dir.path().join("facts"));
    store.write(&fact_with("a", "alpha")).unwrap();
    let mut index = FactIndex::open(&dir.path().join("index.db")).unwrap();
    index.rebuild_if_stale(&[("project", &store)]).unwrap();
    index.rebuild_if_stale(&[("project", &store)]).unwrap();   // no-op, ne panique pas
    std::fs::remove_file(store.dir().join("decision-a.md")).unwrap();
    index.rebuild_if_stale(&[("project", &store)]).unwrap();
    assert!(index.search("alpha", 3).is_empty());               // suppression fichier = sortie d'index
}

#[test]
fn search_survives_fts_special_chars() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = FactIndex::open(&dir.path().join("index.db")).unwrap();
    index.rebuild_if_stale(&[]).unwrap();
    let _ = index.search("query \"with\" AND (specials*", 3);   // ne panique pas
}
```

- [ ] **Step 2: Vérifier l'échec** — `cargo test -p kaji-core --test facts_index_test`.
- [ ] **Step 3: Implémenter** :
  - Schéma (rebuild intégral — les faits se comptent en dizaines, pas en milliers ; pas d'external-content ici) :

```sql
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE VIRTUAL TABLE IF NOT EXISTS facts_fts USING fts5(scope UNINDEXED, file_name UNINDEXED, description, body);
```

  - Fingerprint : pour chaque store, lignes `"{scope}/{file_name}:{mtime_secs}:{len}"` triées puis jointes `;` — comparée à `meta['fingerprint']`. Différente ⇒ `DELETE FROM facts_fts` + réinsertion de tous les faits + update du fingerprint, le tout dans une transaction.
  - `search` : requête MATCH construite en échappant chaque terme entre guillemets doubles — **lire d'abord comment `memory.rs::recall_at` (`memory.rs:320-360`) construit/échappe sa requête MATCH et reproduire la même approche** ; `ORDER BY bm25(facts_fts)` ; requête FTS invalide ⇒ `Vec::new()` (pas de panique).
- [ ] **Step 4: Vert** — `cargo test -p kaji-core --test facts_index_test`, puis `cargo test -p kaji-core` complet.
- [ ] **Step 5: Format + commit** (`git commit -m "feat(facts): index FTS5 dérivé, rebuild par fingerprint mtime, recherche bm25"`)

---

### Task 5: kaji — résolution des scopes (racine git, dirs projet/user, chemin d'index)

**Files:**
- Modify: `crates/kaji/src/config/paths.rs` (accueillir `find_git_root` public), `crates/kaji/src/hints/load_hints.rs` (supprimer sa copie privée `:171-186`, importer la publique), `crates/kaji/src/kaji.rs`
- Test: `crates/kaji/tests/kaji_memory_test.rs` (étendre)

**Interfaces:**
- Consumes: `Paths::in_data_dir` (`config/paths.rs:84-86`), `memory_dir()` (`kaji.rs:39-44`), `kaji_core::facts::slugify`.
- Produces (dans `crate::kaji`) :

```rust
pub fn project_facts_dir(working_dir: &Path) -> PathBuf;
// racine git trouvée -> <racine>/.kaji/memory ; sinon data dir kaji/memory/projects/<slug(chemin absolu)>
pub fn user_facts_dir() -> PathBuf;            // memory_dir().join("user")
pub fn fact_index_path(working_dir: &Path) -> PathBuf;
// memory_dir().join("index").join(format!("{}.db", slug(chemin absolu du working_dir)))
```
et dans `config/paths.rs` : `pub fn find_git_root(start_dir: &Path) -> Option<&Path>` (corps identique à l'actuel privé de `load_hints.rs:171-186`).

- [ ] **Step 1: Tests qui échouent** (dans `kaji_memory_test.rs`, avec `env_lock` + `KAJI_PATH_ROOT` comme le test de migration existant `:182-207`)

```rust
#[test]
fn project_facts_dir_uses_git_root_or_data_dir() {
    let _guard = env_lock::lock_env([("KAJI_PATH_ROOT", Some(tmp.path().to_str().unwrap()))]);
    let repo = tmp.path().join("repo/nested");
    std::fs::create_dir_all(repo.join("../.git")).unwrap();      // .git à repo/
    std::fs::create_dir_all(&repo).unwrap();
    assert_eq!(
        kaji::kaji::project_facts_dir(&repo),
        tmp.path().join("repo/.kaji/memory")
    );
    let outside = tmp.path().join("nogit");
    std::fs::create_dir_all(&outside).unwrap();
    let dir = kaji::kaji::project_facts_dir(&outside);
    assert!(dir.starts_with(tmp.path().join("data")));            // data dir sous KAJI_PATH_ROOT
    assert!(dir.to_string_lossy().contains("projects"));
}
```

(Adapter l'import au vrai nom de crate — voir comment `kaji_memory_test.rs` importe déjà `splice_memory_block` — et le chemin `data` réel produit par `KAJI_PATH_ROOT`, cf. `config/paths.rs`. Ajouter un test analogue pour `fact_index_path` : sous data dir, se termine par `.db`, jamais sous le repo.)

- [ ] **Step 2: Vérifier l'échec** — `cargo test -p kaji --test kaji_memory_test project_facts` : fonctions absentes.
- [ ] **Step 3: Implémenter** — déplacer `find_git_root` dans `paths.rs` (public), le réimporter dans `load_hints.rs` (zéro duplication) ; écrire les 3 fns dans `kaji.rs`. Slug de chemin : `kaji_core::facts::slugify(path.to_string_lossy().as_ref())`.
- [ ] **Step 4: Vert** — `cargo test -p kaji --test kaji_memory_test` + `cargo test -p kaji hints` (non-régression du déplacement).
- [ ] **Step 5: Format + commit** (`git commit -m "feat(memory): résolution scopes faits — racine git, fallback data dir, chemin d'index"`)

---

### Task 6: kaji — curateur (parsing ops, apply avec guards, appel `complete_fast`)

**Files:**
- Create: `crates/kaji/src/memory_curator.rs` (déclarer `pub mod memory_curator;` dans `crates/kaji/src/lib.rs` — vérifier où les modules y sont listés)
- Test: `crates/kaji/tests/memory_curator_test.rs`

**Interfaces:**
- Consumes: `Memory::{uncurated, mark_curated}` (T1), `Fact`/`FactStore` (T2-3), `FactIndex` (T4), dirs (T5), `redact_text` (`session/redact.rs:113`, signature `(input: &str) -> (String, usize)`), `complete_fast` (`model_config.rs:118-156`), `get_fast_model` (`model_config.rs:97-113`), pattern spawn (`context_mgmt/mod.rs:669-706`).
- Produces:

```rust
pub struct CurationOutcome { pub created: usize, pub updated: usize }
pub(crate) fn parse_curator_ops(response: &str) -> anyhow::Result<Vec<CuratorOp>>; // JSON strict, code-fences tolérées
pub(crate) fn apply_ops(
    ops: Vec<CuratorOp>,
    project: &FactStore,
    user: &FactStore,
    session_id: &str,
    today: &str,
) -> CurationOutcome;
pub async fn curate(
    provider: &dyn Provider,
    provider_name: &str,
    model_config: &ModelConfig,
    session_id: &str,
    working_dir: &Path,
) -> anyhow::Result<CurationOutcome>;
pub const CURATOR_CAP: usize = 5;
```

`CuratorOp` (deserialize) : `{ action: "create"|"update", type: "<fact type>", slug, description, body }`.

- [ ] **Step 1: Tests qui échouent** (uniquement les parties pures — aucun réseau, aucun provider)

```rust
#[test]
fn parse_ops_accepts_fenced_json_and_rejects_garbage() {
    let ok = parse_curator_ops("```json\n[{\"action\":\"create\",\"type\":\"gotcha\",\"slug\":\"a\",\"description\":\"d\",\"body\":\"b\"}]\n```").unwrap();
    assert_eq!(ok.len(), 1);
    assert!(parse_curator_ops("désolé, voici les faits en prose").is_err());
}

fn op(action: &str, fact_type: &str, slug: &str, body: &str) -> CuratorOp {
    CuratorOp {
        action: action.into(),
        r#type: fact_type.into(),
        slug: slug.into(),
        description: format!("desc {slug}"),
        body: body.into(),
    }
}

#[test]
fn apply_caps_at_five_routes_by_type_and_redacts_project_scope() {
    let tmp = tempfile::tempdir().unwrap();
    let project = FactStore::new(tmp.path().join("project"));
    let user = FactStore::new(tmp.path().join("user"));
    let mut ops: Vec<CuratorOp> = (0..6)
        .map(|i| op("create", "gotcha", &format!("g{i}"), "corps"))
        .collect();
    ops.push(op("create", "preference", "ma-pref", "indent 4"));
    let outcome = apply_ops(ops, &project, &user, "s1", "2026-08-22");
    assert_eq!(outcome.created, 5);                      // cap: seules les 5 premières
    assert_eq!(project.list().len(), 5);
    assert!(user.list().is_empty());                     // la preference était 7e, cappée

    let secret = op("create", "decision", "avec-secret", "api_key: sk-abcdef1234567890");
    apply_ops(vec![secret], &project, &user, "s1", "2026-08-22");
    let written = project.get(&FactType::Decision, "avec-secret").unwrap();
    assert!(!written.body.contains("sk-abcdef1234567890"));   // redacté en scope projet

    let pref = op("create", "preference", "ma-pref", "indent 4");
    apply_ops(vec![pref], &project, &user, "s1", "2026-08-22");
    assert_eq!(user.list().len(), 1);                    // preference -> scope user
}

#[test]
fn apply_never_touches_created_by_user_and_rejects_bad_slugs() {
    let tmp = tempfile::tempdir().unwrap();
    let project = FactStore::new(tmp.path().join("project"));
    let user = FactStore::new(tmp.path().join("user"));
    let locked = Fact {
        fact_type: FactType::Decision,
        slug: "verrou".into(),
        description: "posé par le user".into(),
        date: "2026-08-01".into(),
        session: "s0".into(),
        created_by: CreatedBy::User,
        body: "corps original".into(),
    };
    project.write(&locked).unwrap();

    let ops = vec![
        op("update", "decision", "verrou", "tentative d'écrasement"),
        op("create", "gotcha", "../evil", "corps"),
    ];
    let outcome = apply_ops(ops, &project, &user, "s1", "2026-08-22");
    assert_eq!(outcome.created + outcome.updated, 0);
    let unchanged = project.get(&FactType::Decision, "verrou").unwrap();
    assert_eq!(unchanged.body, "corps original");
    assert!(matches!(unchanged.created_by, CreatedBy::User));
    assert!(!tmp.path().join("evil").exists());
}
```

- [ ] **Step 2: Vérifier l'échec** — `cargo test -p kaji --test memory_curator_test`.
- [ ] **Step 3: Implémenter** :
  - `parse_curator_ops` : trim, retirer une éventuelle fence ` ```json … ``` `, `serde_json::from_str::<Vec<CuratorOp>>`, valider `action` ∈ {create, update} et `FactType::parse` — la moindre anomalie ⇒ `Err` (fail-closed).
  - `apply_ops` : tronquer à `CURATOR_CAP` ; par op : `FactType::parse` + `validate_slug` sinon skip + `tracing::warn!` ; scope par type (`Preference` ⇒ user, sinon projet) ; scope projet ⇒ `redact_text` sur `description` **et** `body` (garder `.0`) ; si un fait existant même type+slug a `created_by: User` ⇒ skip + warn ; `update` sur fait existant ⇒ conserver `date` et `created_by` d'origine, remplacer `description`/`body` ; `create` ⇒ `created_by: Curator`, `date = today`, `session = session_id` ; compter created/updated.
  - `curate` (orchestration, fail-closed) :
    1. `SessionMemory::load(session_id)` → `store.uncurated(50)` ; vide ⇒ `Ok(CurationOutcome::default())`.
    2. Modèle : env `KAJI_MEMORY_CURATOR_MODEL` d'abord — construire le `ModelConfig` **de la même façon** que `get_fast_model` le fait pour `KAJI_FAST_MODEL` (lire `model_config.rs:81-113` et réutiliser/mirrorer) ; sinon `get_fast_model(provider_name, model_config).await?`.
    3. Prompt système (verbatim) :

```text
You are the memory curator of a coding agent. From the raw journal entries below, extract at most 5 durable facts worth remembering across sessions: decisions (choices with lasting consequences), gotchas (non-obvious traps), preferences (how the user wants things done), references (pointers to resources). Skip small talk, one-off task chatter, and anything already covered by an existing fact unless it needs updating.

Existing facts (type-slug: description):
{existing_facts_index}

Reply with a JSON array only, no prose:
[{"action":"create|update","type":"decision|gotcha|preference|reference","slug":"kebab-case","description":"one line","body":"the fact, self-contained"}]
Reply [] if nothing is worth keeping.
```

    4. Message user = entrées brutes numérotées (`text` de chaque `Entry`). Appel : `complete_fast(provider, &curator_model, session_id, system, &[Message::user().with_text(entries)], &[]).await?` — **vérifier la construction exacte de `Message` user sur l'appelant existant `context_mgmt/mod.rs:621-666` et la reproduire**.
    5. `parse_curator_ops` sur le texte de la réponse (extraction du texte : même méthode que `summarize_tool_call`).
    6. `apply_ops`, puis `FactIndex::open(fact_index_path(working_dir))` + `rebuild_if_stale`, puis **seulement en cas de succès complet** `mark_curated(ids)`.
    7. La moindre `Err` en 2-6 ⇒ propager : rien n'est tamponné, le lot repart au prochain trigger.
- [ ] **Step 4: Vert** — `cargo test -p kaji --test memory_curator_test`.
- [ ] **Step 5: Format + commit** (`git commit -m "feat(memory): curateur LLM — ops JSON fail-closed, cap 5, redaction projet, guard created_by user"`)

---

### Task 7: kaji — trigger fin de tour débouncé aux deux sites (parité)

**Files:**
- Modify: `crates/kaji/src/kaji.rs`, `crates/kaji/src/agents/agent.rs` (~:1018), `crates/kaji/src/agents/state_machine/ops_llm.rs` (~:403)
- Test: `crates/kaji/tests/kaji_memory_test.rs` (étendre)

**Interfaces:**
- Consumes: `curate` (T6), `Memory::uncurated` (T1).
- Produces (dans `crate::kaji`) :

```rust
pub const CURATE_MIN_PENDING: usize = 5;
pub const CURATE_DEBOUNCE_SECS: u64 = 300;
pub fn curation_due(session_id: &str, now_secs: u64) -> bool;
// pending >= CURATE_MIN_PENDING && now - LAST_CURATION_SECS (AtomicU64 statique) >= debounce ;
// si true, met à jour le statique (compare_exchange) — un seul appelant gagne.
pub fn maybe_spawn_curation(
    provider: Arc<dyn Provider>,
    provider_name: String,
    model_config: ModelConfig,
    session_id: String,
    working_dir: PathBuf,
);
// si curation_due: tokio::spawn(curate(...)) ; Ok(o) si o.created+o.updated > 0 =>
// tracing::info!("記 {} faits mémorisés", n) ; Err(e) => tracing::warn!
```

- [ ] **Step 1: Tests qui échouent** (sur `curation_due` seul — pur, sans tokio ni provider)

```rust
#[test]
fn curation_due_needs_threshold_and_debounce() {
    let _guard = env_lock::lock_env([("KAJI_MEMORY_DIR", Some(tmp_dir_str))]);
    // journal vide -> false ; 5 ingests -> true une fois ; immédiatement après -> false (debounce)
}
```

(Le statique de debounce est process-wide : le test doit utiliser un `session_id` dédié et tolérer l'ordre — s'inspirer des tests existants du fichier pour l'isolation `KAJI_MEMORY_DIR`. Si l'état statique rend le test fragile, exposer `curation_due_at(pending: usize, last: u64, now: u64) -> bool` pur et tester celle-là ; `curation_due` devient un thin wrapper non testé.)

- [ ] **Step 2: Vérifier l'échec** — `cargo test -p kaji --test kaji_memory_test curation_due`.
- [ ] **Step 3: Implémenter** `kaji.rs`, puis brancher **la même ligne** aux deux sites, immédiatement **après** l'appel à `splice_memory_block` (jamais avant — la curation ne doit pas racer le recall du tour courant) :
  - `agents/agent.rs` (fin de `prepare_reply_context`, après `:1022`) et `agents/state_machine/ops_llm.rs` (après `:408`) :

```rust
crate::kaji::maybe_spawn_curation(provider, provider_name, model_config, session_id, working_dir);
```

  - Les handles (`provider: Arc<dyn Provider>`, nom, `ModelConfig`, working dir) existent dans les deux contextes — ce sont ceux du flux de reply courant ; regarder comment `context_mgmt::maybe_summarize_tool_pairs` est appelé/alimenté depuis `agent.rs` et faire pareil. Si l'un des deux sites n'a pas un handle sous la main, le remonter depuis la struct porteuse (Agent / state du state-machine) — ne pas recréer un provider.
- [ ] **Step 4: Vert + parité** — `cargo test -p kaji --test kaji_memory_test` ; puis vérifier la parité mécaniquement :

```bash
grep -n "maybe_spawn_curation" crates/kaji/src/agents/agent.rs crates/kaji/src/agents/state_machine/ops_llm.rs
```
Attendu : exactement 1 occurrence dans chaque fichier.
- [ ] **Step 5: Format + commit** (`git commit -m "feat(memory): trigger curation fin de tour — seuil 5, debounce 300s, parité legacy/state-machine"`)

---

### Task 8: kaji — composition du recall (faits curés k=3 en tête du bloc)

**Files:**
- Modify: `crates/kaji/src/kaji.rs` (`splice_memory_block`, `:197-203`)
- Test: `crates/kaji/tests/kaji_memory_test.rs` (étendre)

**Interfaces:**
- Consumes: `FactIndex::search` (T4), dirs (T5), `SessionMemory::recall_prompt` (`kaji.rs:117-138`, inchangé).
- Produces: `splice_memory_block` garde **exactement la même signature** — seule la composition interne change. Nouveau helper privé `fn curated_facts_block(working_dir: &Path, query: &str, k: usize) -> Option<String>`.

- [ ] **Step 1: Tests qui échouent**

```rust
#[test]
fn splice_prepends_curated_facts_then_raw_journal() {
    let _guard = env_lock::lock_env([("KAJI_PATH_ROOT", Some(root)), ("KAJI_MEMORY_DIR", None)]);
    // 1. écrire un fait "decision-cache-ttl" via FactStore dans project_facts_dir(cwd_de_test)
    // 2. ingester une entrée brute contenant "cache" via ingest_turn
    // 3. let out = splice_memory_block("SYSTEM", "s1", "cache ttl");
    //    assert!(out.contains("decision-cache-ttl") || out.contains(<description du fait>));
    //    assert!(le bloc faits apparaît AVANT le bloc journal brut);
    //    assert!(out.starts_with("SYSTEM"));
}

#[test]
fn splice_without_facts_behaves_like_before() {
    // aucun fait : la sortie est identique à l'ancien comportement (recall_prompt seul) —
    // reprendre un cas des tests splice existants du fichier et vérifier la non-régression.
}
```

- [ ] **Step 2: Vérifier l'échec** — `cargo test -p kaji --test kaji_memory_test splice_prepends`.
- [ ] **Step 3: Implémenter** :
  - `curated_facts_block` : `FactIndex::open(fact_index_path(wd))`, `rebuild_if_stale(&[("project", &project_store), ("user", &user_store)])`, `search(query, 3)` ; vide ⇒ `None` ; sinon :

```text
## Faits mémorisés
1. [<type extrait du file_name>] <description> — <body tronqué à 300 chars sur frontière de char>
```

  - `splice_memory_block` : `working_dir = std::env::current_dir()` (fallback `"."`) ; concaténer `system_prompt` + bloc faits (si présent) + bloc `recall_prompt` existant (si présent), séparés par `\n\n`. Toute erreur d'index (io/sqlite) ⇒ bloc faits omis, le splice brut continue (le recall ne casse jamais un tour).
- [ ] **Step 4: Vert** — `cargo test -p kaji --test kaji_memory_test` complet (les anciens tests de splice doivent rester verts tels quels).
- [ ] **Step 5: Format + commit** (`git commit -m "feat(memory): recall — faits curés top-3 en tête du bloc mémoire, journal brut ensuite"`)

---

### Task 9: CLI — `memory list --curated|--raw` + `memory curate`

**Files:**
- Modify: `crates/kaji-cli/src/cli.rs` (`MemoryCommand`, `:757-792`), `crates/kaji-cli/src/commands/memory.rs`
- Test: `crates/kaji-cli/src/commands/memory.rs` (module de test existant, `:150-231`)

**Interfaces:**
- Consumes: `FactStore::list` (T3), dirs (T5), `curate` (T6).
- Produces (clap) :

```rust
List {
    session: Option<String>, limit: Option<usize>, format: String,
    #[arg(long, conflicts_with = "raw")] curated: bool,
    #[arg(long)] raw: bool,
},
Curate,
Clear { ... }   // inchangé
```
Sans flag : comportement actuel (journal brut) — `--raw` est l'alias explicite. `--curated` : table `SCOPE  TYPE  SLUG  DESCRIPTION` sur les deux stores (+ JSON si `format == "json"`).

- [ ] **Step 1: Tests qui échouent** (mêmes conventions que les tests existants du fichier : `KAJI_MEMORY_DIR`/`KAJI_PATH_ROOT` + `env_lock`)

```rust
#[test]
fn curated_listing_merges_both_scopes() {
    // écrire 1 fait user + 1 fait projet via FactStore, appeler la fn de rendu curated,
    // vérifier les deux lignes et leurs scopes.
}
```

- [ ] **Step 2: Vérifier l'échec** — `cargo test -p kaji-cli memory`.
- [ ] **Step 3: Implémenter** — rendu curated dans `commands/memory.rs` (fn pure testable + branchement dans `list`) ; `Curate` : construire provider + `ModelConfig` **exactement comme une commande CLI existante qui parle au LLM le fait** (chercher l'usage de `create_provider`/équivalent dans `kaji-cli` — celui du chemin `run`/session) ; si aucun provider configuré ⇒ erreur claire `anyhow::bail!("aucun provider configuré — lancez `kaji configure`")`. Succès ⇒ `println!("記 {} faits mémorisés", n)` (0 inclus : `記 0 faits mémorisés — rien à curer`).
- [ ] **Step 4: Vert** — `cargo test -p kaji-cli`.
- [ ] **Step 5: Format + commit** (`git commit -m "feat(cli): kaji memory list --curated/--raw et kaji memory curate"`)

---

### Task 10: `/remember` in-session (écriture immédiate 0-token)

**Files:**
- Modify: `crates/kaji/src/agents/execute_commands.rs` (`COMMANDS` `:20-58`, dispatch `:121-163`, nouveau handler)
- Test: `crates/kaji/tests/kaji_memory_test.rs` ou module de test d'`execute_commands.rs` — suivre l'emplacement des tests existants des commandes (`grep -n "mod tests" crates/kaji/src/agents/execute_commands.rs`)

**Interfaces:**
- Consumes: `Fact`/`FactStore`/`slugify` (T2-3), dirs (T5), `maybe_spawn_curation` (T7).
- Produces: commande `remember` dans le registre (`CommandDef { name: "remember", description: "Save a durable note to memory" }`) ; parsing pur exposé pour test :

```rust
pub(crate) fn parse_remember_note(args: &str) -> (FactType, String);
// "decision: on garde X" -> (Decision, "on garde X") ; sans préfixe -> (Preference, note entière)
```

- [ ] **Step 1: Tests qui échouent**

```rust
#[test]
fn remember_note_parses_optional_type_prefix() {
    assert!(matches!(parse_remember_note("gotcha: rm est aliasé").0, FactType::Gotcha));
    assert!(matches!(parse_remember_note("juste une note").0, FactType::Preference));
    assert_eq!(parse_remember_note("decision: choix A").1, "choix A");
}
```

- [ ] **Step 2: Vérifier l'échec** — `cargo test -p kaji remember_note`.
- [ ] **Step 3: Implémenter** :
  - Regarder comment une commande existante **avec arguments** est parsée dans `execute_command` (`:121-163` — ex. `goal`) et reproduire le même mécanisme pour extraire le texte après `/remember`.
  - Handler : note vide ⇒ message d'usage ; sinon `(fact_type, text) = parse_remember_note(...)` ; `slug = slugify(text)` (si vide après slugify : `format!("note-{session_id}")` tronqué) ; `Fact { created_by: User, date: <chrono::Local aujourd'hui — vérifier que chrono est déjà une dep de kaji via grep Cargo.toml, sinon utiliser l'utilitaire de date déjà présent dans le crate>, session: session_id, description: première ligne tronquée à 120 chars, body: text }` ; scope par type (comme T6) ; **aucune redaction pour created_by user en scope user ; redaction quand même en scope projet** ; `FactStore::write` ; rebuild index ; puis `maybe_spawn_curation(...)` ; retour `Ok(Some(Message))` dont le texte est `記 fait mémorisé : <file_name>` — construire le `Message` comme les autres handlers du fichier le font.
- [ ] **Step 4: Vert** — `cargo test -p kaji remember` ; vérifier aussi que `/remember` apparaît dans `list_commands()` (test une ligne si le fichier en a déjà pour le registre).
- [ ] **Step 5: Format + commit** (`git commit -m "feat(memory): /remember — écriture immédiate d'un fait created_by user, curation déclenchée"`)

---

### Task 11: retrait extension MCP memory + migration one-shot des `.txt`

**Files:**
- Delete: `crates/kaji-mcp/src/memory/mod.rs` (via `git rm -r crates/kaji-mcp/src/memory`)
- Modify: `crates/kaji-mcp/src/lib.rs` (`:17,25` mod/pub use ; `:61` entrée `builtin!(memory, MemoryServer)`), `crates/kaji-cli/src/commands/configure.rs` (`:1177-1181`, retirer la ligne memory de la liste), `crates/kaji/src/config/extensions.rs` (`:486-492`, les tests référencent `"memory"` par nom → les basculer sur un autre builtin existant, ex. `developer`), `crates/kaji/src/kaji.rs` (migration)
- Test: `crates/kaji/tests/kaji_memory_test.rs` (étendre)

**Interfaces:**
- Consumes: format `.txt` hérité — entrées séparées par ligne vide, première ligne optionnelle `# tag1 tag2` (cf. l'actuel `memory/mod.rs:205-270` **avant** suppression : lire pour confirmer le parsing) ; `FactStore` (T3) ; dirs (T5) ; chemin global hérité = **config dir** `kaji/memory` (pas data dir — voir `memory/mod.rs:105-107` avant suppression, reproduire sa résolution).
- Produces (dans `crate::kaji`) : `pub fn migrate_legacy_txt_memory(working_dir: &Path)` — appelée au début de `project_facts_dir`… non : appelée une fois par process, au premier `splice_memory_block` (même veine que `migrate_legacy_sessions` dans `SessionMemory::load`, `kaji.rs:62-72`). Utiliser `std::sync::Once`.

- [ ] **Step 1: Tests qui échouent**

```rust
#[test]
fn legacy_txt_categories_become_reference_facts_and_are_renamed() {
    let _guard = env_lock::lock_env([("KAJI_PATH_ROOT", Some(root))]);
    // déposer <repo>/.kaji/memory/workflow.txt avec 2 entrées ("# tag\ndata1\n\ndata2\n") ;
    // appeler migrate_legacy_txt_memory(repo) ;
    // attendu : reference-workflow.md existe (created_by curator, body contenant data1 et data2,
    // tags rendus en préfixe de ligne), workflow.txt renommé workflow.txt.legacy ;
    // 2e appel : no-op (idempotent, .legacy ignoré).
}
```

- [ ] **Step 2: Vérifier l'échec** — `cargo test -p kaji --test kaji_memory_test legacy_txt`.
- [ ] **Step 3: Implémenter la migration** (avant de supprimer l'extension — son code sert de référence de parsing) : scanner `project_facts_dir` candidat (`<racine>/.kaji/memory/*.txt`) → faits scope projet ; scanner le dossier global hérité → faits scope user ; un fait `reference` par fichier catégorie, slug = `slugify(nom sans extension)`, description = `format!("Importé de l'extension memory héritée ({n} entrées)")` ; rename `*.txt` → `*.txt.legacy` (jamais de suppression) ; erreurs par-fichier : warn + continue.
- [ ] **Step 4: Retirer l'extension** — `git rm -r crates/kaji-mcp/src/memory` + purger `lib.rs`/`configure.rs`/`extensions.rs` comme listé. Puis :

Run: `cargo build` puis `cargo test -p kaji-mcp -p kaji --tests` — attendu : compile, tests verts, plus aucune référence (`grep -rn "MemoryServer" crates/ | grep -v target` vide).
- [ ] **Step 5: Format + commit** (`git commit -m "feat(memory)!: retrait extension MCP memory héritée, migration one-shot .txt vers faits md"`)

---

### Task 12: E2E deux sessions + `kaji-self-test.yaml` + clippy final

**Files:**
- Test: `crates/kaji/tests/kaji_memory_test.rs` (étendre)
- Modify: `kaji-self-test.yaml` (bloc `instructions:`, ajouter une phase mémoire en prose)

**Interfaces:**
- Consumes: tout ce qui précède.

- [ ] **Step 1: Test e2e deux sessions** (sans LLM — le chemin curateur JSON est déjà couvert en T6 ; ici on prouve la persistance cross-session du recall)

```rust
#[test]
fn fact_written_in_session_one_is_recalled_in_session_two() {
    let _guard = env_lock::lock_env([("KAJI_PATH_ROOT", Some(root))]);
    // "session 1" : FactStore::write d'un fait decision-choix-provider + rebuild index
    //   (passer par les fns publiques : project_facts_dir + FactIndex, pas de raccourci privé)
    // "session 2" : nouvel appel splice_memory_block("SYS", "s2", "choix provider")
    //   -> contient la description du fait. Aucun état partagé en mémoire process : tout
    //   passe par le disque, c'est le point du test.
}
```

- [ ] **Step 2: Vert** — `cargo test -p kaji --test kaji_memory_test`.
- [ ] **Step 3: `kaji-self-test.yaml`** — dans `instructions:`, ajouter une `### Phase 6 — Memory` en prose (format des phases existantes, lignes 68-105) : demander à l'agent d'exécuter `/remember gotcha: self-test memory probe`, puis `kaji memory list --curated` et vérifier que le fait apparaît, puis supprimer le fichier créé. Mentionner la phase dans le paramètre `test_phases` si sa description énumère les phases.
- [ ] **Step 4: Gate qualité finale (toute la feature)**

```bash
cargo build
cargo test -p kaji-core -p kaji -p kaji-cli -p kaji-mcp
cargo clippy --all-targets -- -D warnings
```
Attendu : tout vert, zéro warning. (Le run réel `kaji run --recipe kaji-self-test.yaml` nécessite un binaire installé et des tokens API — le proposer au user, ne pas le lancer sans go.)
- [ ] **Step 5: Format + commit** (`git commit -m "test(memory): e2e recall cross-session + phase memory du self-test"`)
