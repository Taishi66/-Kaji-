//! Goal session state (item 5 ante) : un but tenu par le client, un
//! évaluateur qui juge après chaque tour de travail, et un cap d'itérations
//! comme backstop de la boucle non supervisée. Même partage que `sdd` :
//! l'état pur vit ici, le client rend et déclenche les transitions.

pub const DEFAULT_MAX_ITERATIONS: usize = 10;

/// Le retour de l'évaluateur est réinjecté dans le prompt de continuation —
/// sans borne, un évaluateur bavard ferait grossir chaque tour suivant.
pub const MAX_FEEDBACK_CHARS: usize = 2_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalPhase {
    Working,
    Evaluating,
}

impl GoalPhase {
    pub fn label(&self) -> &'static str {
        match self {
            GoalPhase::Working => "working",
            GoalPhase::Evaluating => "evaluating",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalOutcome {
    Met,
    Unreachable,
    Cleared,
    Interrupted,
    IterationCap,
}

impl GoalOutcome {
    pub fn label(&self) -> &'static str {
        match self {
            GoalOutcome::Met => "met",
            GoalOutcome::Unreachable => "unreachable",
            GoalOutcome::Cleared => "cleared",
            GoalOutcome::Interrupted => "interrupted",
            GoalOutcome::IterationCap => "iteration cap",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Met,
    Continue(String),
    Unreachable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalStep {
    Continue(String),
    Finished(GoalOutcome),
}

#[derive(Debug, Clone)]
pub struct GoalState {
    pub condition: String,
    pub iteration: usize,
    pub max_iterations: usize,
    pub phase: GoalPhase,
    pub outcome: Option<GoalOutcome>,
}

impl GoalState {
    pub fn new(condition: String, max_iterations: usize) -> Self {
        Self {
            condition,
            iteration: 1,
            max_iterations,
            phase: GoalPhase::Working,
            outcome: None,
        }
    }

    pub fn is_active(&self) -> bool {
        self.outcome.is_none()
    }

    pub fn begin_evaluation(&mut self) {
        self.phase = GoalPhase::Evaluating;
    }

    /// Une itération = un tour de travail + un tour d'évaluation : le cap est
    /// donc lu au moment où l'évaluateur demande un tour de plus, pas au
    /// démarrage du tour de travail.
    pub fn apply_verdict(&mut self, verdict: Verdict) -> GoalStep {
        match verdict {
            Verdict::Met => self.finished(GoalOutcome::Met),
            Verdict::Unreachable(_) => self.finished(GoalOutcome::Unreachable),
            Verdict::Continue(feedback) => {
                if self.iteration >= self.max_iterations {
                    self.finished(GoalOutcome::IterationCap)
                } else {
                    self.iteration += 1;
                    self.phase = GoalPhase::Working;
                    GoalStep::Continue(feedback)
                }
            }
        }
    }

    pub fn finish(&mut self, outcome: GoalOutcome) {
        self.outcome = Some(outcome);
    }

    fn finished(&mut self, outcome: GoalOutcome) -> GoalStep {
        self.finish(outcome);
        GoalStep::Finished(outcome)
    }
}

/// `KAJI_GOAL_MAX_ITERATIONS`, lu par l'appelant : une valeur absente,
/// illisible ou nulle retombe sur le défaut plutôt que de désarmer le
/// backstop.
pub fn max_iterations(raw: Option<&str>) -> usize {
    raw.and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_ITERATIONS)
}

pub fn work_prompt(condition: &str) -> String {
    format!(
        "Goal: {condition}\n\nStart (or keep) working toward this goal. When you believe it is done, stop and summarise what was done."
    )
}

/// Réfutation active, dérivée du juge SDD : le biais par défaut est CONTINUE,
/// et l'évaluateur dispose des outils pour vérifier au lieu de supposer.
pub fn evaluator_prompt(condition: &str) -> String {
    format!(
        "You are a goal evaluator, not an agreeable assistant: your default bias must be CONTINUE, not MET. Goal to judge: {condition}\n\nCheck it actively against the real state of the project — run whatever tools you need (tests, file reads, commands) to prove it instead of assuming it. Conclude MET only if the goal is verifiably reached, with the demonstration to back it; at the slightest doubt, or on any missing evidence, the verdict is CONTINUE and you list precisely what is left to do. Conclude UNREACHABLE only if the goal is intrinsically unreachable (contradictory, out of the project's scope, dependent on something unavailable) — never because it is hard or long. Justify your verdict before the final line: everything preceding it is the feedback carried into the next turn. Last line, exactly: `VERDICT: MET` or `VERDICT: CONTINUE` or `VERDICT: UNREACHABLE`."
    )
}

pub fn continuation_prompt(condition: &str, feedback: &str) -> String {
    format!(
        "The goal is not met yet. Evaluator feedback:\n{feedback}\n\nKeep working toward: {condition}"
    )
}

/// Le sous-scan partagé avec le juge SDD de la TUI, dont la taxonomie diffère
/// (`VALID`/`DRIFT`) mais qui lit la même ligne : index de la dernière ligne
/// non vide, et son contenu en majuscules.
pub fn last_verdict_line(text: &str) -> Option<(usize, String)> {
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .last()
        .map(|(index, line)| (index, line.to_uppercase()))
}

/// Verdict = dernière ligne non vide, retour = les lignes qui la précèdent.
/// `None` pour une sortie sans ligne de verdict reconnaissable : l'appelant
/// continue par prudence plutôt que de lire un silence comme un succès.
pub fn parse_verdict(text: &str) -> Option<Verdict> {
    let (last_index, last_line) = last_verdict_line(text)?;
    let feedback = || {
        let above = text.lines().take(last_index).collect::<Vec<_>>().join("\n");
        bound_feedback(above.trim())
    };
    if last_line.contains("VERDICT: MET") {
        Some(Verdict::Met)
    } else if last_line.contains("VERDICT: UNREACHABLE") {
        Some(Verdict::Unreachable(feedback()))
    } else if last_line.contains("VERDICT: CONTINUE") {
        Some(Verdict::Continue(feedback()))
    } else {
        None
    }
}

pub fn bound_feedback(text: &str) -> String {
    let total = text.chars().count();
    if total <= MAX_FEEDBACK_CHARS {
        return text.to_string();
    }
    let head: String = text.chars().take(MAX_FEEDBACK_CHARS - 1).collect();
    format!("{head}…")
}
