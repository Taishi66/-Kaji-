use kaji_core::sdd::{SddPass, SddStage, SpecDoc, StageStatus};
use std::path::PathBuf;

#[test]
fn parse_extracts_title_from_first_h1() {
    let doc = SpecDoc::parse(PathBuf::from("SPEC.md"), "intro\n# Ma Spec\ncorps");
    assert_eq!(doc.title, "Ma Spec");
    assert!(doc.body.contains("corps"));
}

#[test]
fn parse_falls_back_to_file_stem_without_h1() {
    let doc = SpecDoc::parse(PathBuf::from("demo-spec.md"), "pas de titre ici");
    assert_eq!(doc.title, "demo-spec");
}

#[test]
fn load_missing_file_errors() {
    assert!(SpecDoc::load(std::path::Path::new("/nonexistent/SPEC.md")).is_err());
}

#[test]
fn empty_spec_is_detected() {
    assert!(SpecDoc::parse(PathBuf::from("s.md"), "  \n\t").is_empty());
}

#[test]
fn new_pass_is_all_pending_and_idle() {
    let pass = SddPass::new();
    assert!(pass.current().is_none());
    assert!(!pass.is_running());
    assert!(pass
        .stages()
        .iter()
        .all(|(_, s)| *s == StageStatus::Pending));
}

#[test]
fn start_puts_intent_running() {
    let mut pass = SddPass::new();
    pass.start();
    assert_eq!(pass.current(), Some(SddStage::Intent));
    assert_eq!(pass.stages()[0], (SddStage::Intent, StageStatus::Running));
}

#[test]
fn advance_walks_all_stages_to_completion() {
    let mut pass = SddPass::new();
    pass.start();
    for _ in 0..6 {
        pass.advance();
    }
    assert!(pass.is_complete());
    assert!(!pass.drifted());
    assert!(pass.current().is_none());
    assert!(pass.stages().iter().all(|(_, s)| *s == StageStatus::Done));
}

#[test]
fn fail_current_stops_the_pass_and_marks_drift() {
    let mut pass = SddPass::new();
    pass.start();
    pass.advance();
    pass.advance();
    assert_eq!(pass.current(), Some(SddStage::Gate));
    pass.fail_current();
    assert!(pass.drifted());
    assert!(!pass.is_running());
    assert_eq!(pass.stages()[2], (SddStage::Gate, StageStatus::Failed));
    pass.advance();
    assert!(pass.current().is_none());
}

#[test]
fn start_twice_is_a_noop_while_running() {
    let mut pass = SddPass::new();
    pass.start();
    pass.advance();
    pass.start();
    assert_eq!(pass.current(), Some(SddStage::Spec));
}
