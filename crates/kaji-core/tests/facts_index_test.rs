use kaji_core::facts::{CreatedBy, Fact, FactIndex, FactStore, FactType};

fn fact_with(slug: &str, body: &str) -> Fact {
    let description = body
        .split_whitespace()
        .take(4)
        .collect::<Vec<_>>()
        .join(" ");
    Fact {
        fact_type: FactType::Decision,
        slug: slug.into(),
        description,
        date: "2026-08-22".into(),
        session: "s1".into(),
        created_by: CreatedBy::Curator,
        body: body.into(),
    }
}

#[test]
fn rebuild_indexes_and_search_ranks_by_match() {
    let dir = tempfile::tempdir().unwrap();
    let store = FactStore::new(dir.path().join("facts"));
    store
        .write(&fact_with("cache-ttl", "Le TTL du cache est une heure"))
        .unwrap();
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
    index.rebuild_if_stale(&[("project", &store)]).unwrap(); // no-op, ne panique pas
    std::fs::remove_file(store.dir().join("decision-a.md")).unwrap();
    index.rebuild_if_stale(&[("project", &store)]).unwrap();
    assert!(index.search("alpha", 3).is_empty()); // suppression fichier = sortie d'index
}

#[test]
fn search_survives_fts_special_chars() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = FactIndex::open(&dir.path().join("index.db")).unwrap();
    index.rebuild_if_stale(&[]).unwrap();
    let _ = index.search("query \"with\" AND (specials*", 3); // ne panique pas
}
