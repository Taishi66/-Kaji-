use kaji_core::goal::{
    self, DEFAULT_MAX_ITERATIONS, GoalOutcome, GoalPhase, GoalState, GoalStep, MAX_FEEDBACK_CHARS,
    Verdict,
};

#[test]
fn a_fresh_goal_starts_working_on_its_first_iteration() {
    let goal = GoalState::new("les tests passent".to_string(), 10);

    assert_eq!(goal.iteration, 1);
    assert_eq!(goal.phase, GoalPhase::Working);
    assert_eq!(goal.outcome, None);
    assert!(goal.is_active());
}

#[test]
fn verdict_met_is_read_from_the_last_non_empty_line() {
    let verdict = goal::parse_verdict("j'ai lancé les tests\n\nVERDICT: MET\n\n");

    assert_eq!(verdict, Some(Verdict::Met));
}

#[test]
fn verdict_continue_carries_the_lines_above_it_as_feedback() {
    let verdict =
        goal::parse_verdict("il manque le cas nul\net un test de bord\nVERDICT: CONTINUE");

    assert_eq!(
        verdict,
        Some(Verdict::Continue(
            "il manque le cas nul\net un test de bord".to_string()
        ))
    );
}

#[test]
fn verdict_unreachable_carries_its_reason() {
    let verdict = goal::parse_verdict("le dépôt n'a pas de suite de tests\nVERDICT: UNREACHABLE");

    assert_eq!(
        verdict,
        Some(Verdict::Unreachable(
            "le dépôt n'a pas de suite de tests".to_string()
        ))
    );
}

/// ⛔ BARRIÈRE — un verdict absent ou imparsable ne doit jamais être lu comme
/// MET : `parse_verdict` renvoie `None` et l'appelant continue par prudence.
#[test]
fn an_absent_or_unparsable_verdict_is_none() {
    assert_eq!(goal::parse_verdict("j'ai fini, c'est bon"), None);
    assert_eq!(goal::parse_verdict(""), None);
    assert_eq!(goal::parse_verdict("VERDICT: PEUT-ÊTRE"), None);
}

/// La ligne de verdict est la DERNIÈRE non vide : un `VERDICT: MET` cité en
/// milieu de raisonnement ne conclut pas le tour.
#[test]
fn only_the_last_non_empty_line_decides_the_verdict() {
    let verdict =
        goal::parse_verdict("je pourrais écrire VERDICT: MET mais non\nVERDICT: CONTINUE");

    assert!(matches!(verdict, Some(Verdict::Continue(_))));
}

#[test]
fn feedback_is_bounded_to_two_thousand_chars() {
    let long = "é".repeat(5_000);
    let text = format!("{long}\nVERDICT: CONTINUE");

    let Some(Verdict::Continue(feedback)) = goal::parse_verdict(&text) else {
        panic!("un verdict CONTINUE explicite");
    };

    assert!(feedback.chars().count() <= MAX_FEEDBACK_CHARS);
    assert!(goal::bound_feedback(&long).chars().count() <= MAX_FEEDBACK_CHARS);
    assert_eq!(goal::bound_feedback("court"), "court");
}

#[test]
fn continue_advances_the_iteration_and_goes_back_to_working() {
    let mut goal = GoalState::new("but".to_string(), 3);
    goal.begin_evaluation();
    assert_eq!(goal.phase, GoalPhase::Evaluating);

    let step = goal.apply_verdict(Verdict::Continue("il reste X".to_string()));

    assert_eq!(step, GoalStep::Continue("il reste X".to_string()));
    assert_eq!(goal.iteration, 2);
    assert_eq!(goal.phase, GoalPhase::Working);
    assert!(goal.is_active());
}

#[test]
fn met_finishes_the_goal() {
    let mut goal = GoalState::new("but".to_string(), 3);
    goal.begin_evaluation();

    let step = goal.apply_verdict(Verdict::Met);

    assert_eq!(step, GoalStep::Finished(GoalOutcome::Met));
    assert_eq!(goal.outcome, Some(GoalOutcome::Met));
    assert!(!goal.is_active());
}

#[test]
fn unreachable_finishes_the_goal() {
    let mut goal = GoalState::new("but".to_string(), 3);
    goal.begin_evaluation();

    let step = goal.apply_verdict(Verdict::Unreachable("impossible".to_string()));

    assert_eq!(step, GoalStep::Finished(GoalOutcome::Unreachable));
    assert_eq!(goal.outcome, Some(GoalOutcome::Unreachable));
}

/// ⛔ BARRIÈRE — le cap est le backstop d'une boucle non supervisée : un
/// évaluateur qui répond CONTINUE indéfiniment doit s'arrêter tout seul.
#[test]
fn the_iteration_cap_stops_an_endless_continue_loop() {
    let mut goal = GoalState::new("but".to_string(), 2);

    goal.begin_evaluation();
    assert_eq!(
        goal.apply_verdict(Verdict::Continue("encore".to_string())),
        GoalStep::Continue("encore".to_string())
    );
    assert_eq!(goal.iteration, 2);

    goal.begin_evaluation();
    let step = goal.apply_verdict(Verdict::Continue("toujours".to_string()));

    assert_eq!(step, GoalStep::Finished(GoalOutcome::IterationCap));
    assert_eq!(goal.outcome, Some(GoalOutcome::IterationCap));
    assert_eq!(goal.iteration, 2, "le cap ne franchit pas le maximum");
}

#[test]
fn finish_records_an_external_outcome() {
    let mut goal = GoalState::new("but".to_string(), 3);

    goal.finish(GoalOutcome::Cleared);

    assert_eq!(goal.outcome, Some(GoalOutcome::Cleared));
    assert!(!goal.is_active());
}

#[test]
fn max_iterations_defaults_when_the_env_value_is_absent_or_invalid() {
    assert_eq!(goal::max_iterations(None), DEFAULT_MAX_ITERATIONS);
    assert_eq!(goal::max_iterations(Some("")), DEFAULT_MAX_ITERATIONS);
    assert_eq!(goal::max_iterations(Some("zéro")), DEFAULT_MAX_ITERATIONS);
    assert_eq!(goal::max_iterations(Some("0")), DEFAULT_MAX_ITERATIONS);
    assert_eq!(goal::max_iterations(Some("3")), 3);
    assert_eq!(goal::max_iterations(Some(" 3 ")), 3);
}

#[test]
fn prompts_carry_the_condition_and_the_feedback() {
    let work = goal::work_prompt("les tests passent");
    assert!(work.contains("Objectif : les tests passent"));
    assert!(work.contains("Commence (ou continue) à travailler"));

    let evaluator = goal::evaluator_prompt("les tests passent");
    assert!(evaluator.contains("les tests passent"));
    assert!(evaluator.contains("VERDICT: MET"));
    assert!(evaluator.contains("VERDICT: CONTINUE"));
    assert!(evaluator.contains("VERDICT: UNREACHABLE"));

    let continuation = goal::continuation_prompt("les tests passent", "il reste X");
    assert!(continuation.contains("il reste X"));
    assert!(continuation.contains("Continue le travail vers : les tests passent"));
}

#[test]
fn phases_and_outcomes_have_display_labels() {
    assert_eq!(GoalPhase::Working.label(), "travail");
    assert_eq!(GoalPhase::Evaluating.label(), "évaluation");
    assert_eq!(GoalOutcome::Met.label(), "atteint");
    assert_eq!(GoalOutcome::Unreachable.label(), "inatteignable");
    assert_eq!(GoalOutcome::Cleared.label(), "effacé");
    assert_eq!(GoalOutcome::Interrupted.label(), "interrompu");
    assert_eq!(GoalOutcome::IterationCap.label(), "cap d'itérations");
}
