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
            GoalPhase::Working => "travail",
            GoalPhase::Evaluating => "évaluation",
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
            GoalOutcome::Met => "atteint",
            GoalOutcome::Unreachable => "inatteignable",
            GoalOutcome::Cleared => "effacé",
            GoalOutcome::Interrupted => "interrompu",
            GoalOutcome::IterationCap => "cap d'itérations",
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
        "Objectif : {condition}\n\nCommence (ou continue) à travailler vers cet objectif. Quand tu penses avoir terminé, arrête-toi et résume ce qui a été fait."
    )
}

/// Réfutation active, dérivée du juge SDD : le biais par défaut est CONTINUE,
/// et l'évaluateur dispose des outils pour vérifier au lieu de supposer.
pub fn evaluator_prompt(condition: &str) -> String {
    format!(
        "Tu es un évaluateur de but, pas un assistant complaisant : ton biais par défaut doit être CONTINUE, pas MET. But à juger : {condition}\n\nVérifie-le activement contre l'état réel du projet — lance les outils dont tu as besoin (tests, lecture de fichiers, commandes) pour le prouver au lieu de le supposer. Ne conclus MET que si le but est vérifiablement atteint, démonstration à l'appui ; au moindre doute ou à la moindre preuve manquante, le verdict est CONTINUE et tu listes précisément ce qu'il reste à faire. Ne conclus UNREACHABLE que si le but est intrinsèquement inatteignable (contradictoire, hors du périmètre du projet, dépendant de quelque chose d'indisponible) — jamais parce que c'est difficile ou long. Justifie ton verdict avant la ligne finale : tout ce qui la précède est le retour transmis au tour suivant. Dernière ligne, exactement : `VERDICT: MET` ou `VERDICT: CONTINUE` ou `VERDICT: UNREACHABLE`."
    )
}

pub fn continuation_prompt(condition: &str, feedback: &str) -> String {
    format!(
        "Le but n'est pas encore atteint. Retour de l'évaluateur :\n{feedback}\n\nContinue le travail vers : {condition}"
    )
}

/// Verdict = dernière ligne non vide, retour = les lignes qui la précèdent.
/// `None` pour une sortie sans ligne de verdict reconnaissable : l'appelant
/// continue par prudence plutôt que de lire un silence comme un succès.
pub fn parse_verdict(text: &str) -> Option<Verdict> {
    let (last_index, last_line) = text
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .last()?;
    let last_line = last_line.to_uppercase();
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
