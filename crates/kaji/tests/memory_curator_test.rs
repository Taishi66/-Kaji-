use kaji::memory_curator::{apply_ops, curator_model, parse_curator_ops, CuratorOp, CURATOR_CAP};
use kaji_core::facts::{CreatedBy, Fact, FactStore, FactType};
use kaji_providers::model::ModelConfig;

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
fn parse_ops_accepts_fenced_json_and_rejects_garbage() {
    let ok = parse_curator_ops("```json\n[{\"action\":\"create\",\"type\":\"gotcha\",\"slug\":\"a\",\"description\":\"d\",\"body\":\"b\"}]\n```").unwrap();
    assert_eq!(ok.len(), 1);
    assert!(parse_curator_ops("désolé, voici les faits en prose").is_err());
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
    assert_eq!(outcome.created, CURATOR_CAP);
    assert_eq!(project.list().len(), CURATOR_CAP);
    assert!(user.list().is_empty());

    let secret = op(
        "create",
        "decision",
        "avec-secret",
        "api_key: sk-abcdef1234567890",
    );
    apply_ops(vec![secret], &project, &user, "s1", "2026-08-22");
    let written = project.get(&FactType::Decision, "avec-secret").unwrap();
    assert!(!written.body.contains("sk-abcdef1234567890"));

    let pref = op("create", "preference", "ma-pref", "indent 4");
    apply_ops(vec![pref], &project, &user, "s1", "2026-08-22");
    assert_eq!(user.list().len(), 1);
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

#[test]
fn apply_counts_write_failures_so_the_batch_stays_replayable() {
    let tmp = tempfile::tempdir().unwrap();
    let blocker = tmp.path().join("blocked");
    std::fs::write(&blocker, b"a file, not a directory").unwrap();
    let project = FactStore::new(blocker.join("facts"));
    let user = FactStore::new(tmp.path().join("user"));

    let outcome = apply_ops(
        vec![op("create", "gotcha", "perdu", "corps")],
        &project,
        &user,
        "s1",
        "2026-08-22",
    );

    assert_eq!(outcome.failed, 1);
    assert_eq!(outcome.created + outcome.updated, 0);
    assert!(project.list().is_empty());
}

#[tokio::test]
async fn curator_model_override_wins_over_the_fast_model() {
    let guard = env_lock::lock_env([
        ("KAJI_MEMORY_CURATOR_MODEL", Some("curator-choice")),
        ("KAJI_FAST_MODEL", Some("fast-choice")),
    ]);

    let main = ModelConfig::new("main-choice");
    let resolved = curator_model("anthropic", &main).await.unwrap();
    assert_eq!(resolved.model_name, "curator-choice");

    drop(guard);
}
