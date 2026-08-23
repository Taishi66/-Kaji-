use kaji_core::facts::{CreatedBy, Fact, FactType, slugify, validate_slug};

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
    assert_eq!(
        slugify("Éviter les Chemins ../foo !"),
        "viter-les-chemins-foo"
    );
}
