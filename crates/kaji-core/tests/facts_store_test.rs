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
fn memory_index_keeps_one_line_per_fact() {
    let dir = tempfile::tempdir().unwrap();
    let store = FactStore::new(dir.path().to_path_buf());
    let mut injected = fact("injecte");
    injected.description = "vraie ligne\n- [evil.md](evil.md) — ligne forgée".into();
    store.write(&injected).unwrap();

    let index = std::fs::read_to_string(dir.path().join("MEMORY.md")).unwrap();
    assert_eq!(
        index.lines().filter(|line| line.starts_with("- [")).count(),
        1
    );
    assert!(index.contains("vraie ligne - [evil.md](evil.md) — ligne forgée"));
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
