//! Le volet 炉 forge — ce que les lames déléguées sont en train de faire.
//!
//! Deux sources alimentent le même état, et elles ne disent pas la même chose :
//! le snapshot d'`Agent::subagent_snapshot` (tick 1 s) fait autorité sur le
//! statut, les tours et le chrono ; la notification `subagent_tool_request`,
//! elle, est la seule à savoir quel outil brûle à l'instant. La réconciliation
//! ci-dessous respecte ce partage — le snapshot n'efface jamais l'outil courant
//! d'une tâche encore vivante, la notification ne change jamais un statut.

use kaji::agents::{SubagentTaskSnapshot, SubagentTaskStatus};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// Combien de temps le volet survit à sa dernière tâche : le temps de lire le
/// verdict, pas plus. Passé ce délai le mode `Auto` le replie tout seul.
const FORGE_FOLD: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgeStatus {
    Running,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct ForgeTask {
    pub id: String,
    pub description: String,
    pub status: ForgeStatus,
    pub current_tool: Option<String>,
    pub elapsed_secs: u64,
    pub turns: u32,
    pub result: Option<String>,
    pub error: Option<String>,
}

/// `Auto` laisse le volet suivre l'activité ; les deux autres sont la main de
/// l'utilisateur (Ctrl+F, `/forge`), qui gagne jusqu'à la prochaine tâche.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ForgeView {
    #[default]
    Auto,
    ForcedOpen,
    ForcedClosed,
}

#[derive(Debug, Default)]
pub struct ForgeState {
    pub tasks: BTreeMap<String, ForgeTask>,
    pub selected: usize,
    pub view: ForgeView,
    /// Instant après lequel le mode `Auto` replie le volet — posé une seule
    /// fois, quand la dernière tâche vivante s'arrête.
    pub folds_at: Option<Instant>,
}

impl ForgeState {
    /// Le snapshot fait autorité sur le statut. Une tâche annulée localement
    /// ne repasse pas `Running` le temps que summon la retire, et une tâche
    /// `Running` qui disparaît du snapshot finit `Done` — aucune ligne ne
    /// s'évapore sans état final.
    pub fn apply_snapshot(&mut self, snap: Vec<SubagentTaskSnapshot>) {
        let had_running = self.running_count() > 0;
        let seen: Vec<String> = snap.iter().map(|entry| entry.id.clone()).collect();

        for entry in snap {
            match self.tasks.get_mut(&entry.id) {
                Some(task) => {
                    task.description = entry.description;
                    task.turns = entry.turns;
                    task.elapsed_secs = entry.elapsed_secs;
                    task.result = entry.result;
                    task.error = entry.error;
                    if task.status != ForgeStatus::Cancelled {
                        task.status = reconciled_status(entry.status);
                    }
                    if task.status != ForgeStatus::Running {
                        task.current_tool = None;
                    }
                }
                None => {
                    self.insert(ForgeTask {
                        id: entry.id,
                        description: entry.description,
                        status: reconciled_status(entry.status),
                        current_tool: None,
                        elapsed_secs: entry.elapsed_secs,
                        turns: entry.turns,
                        result: entry.result,
                        error: entry.error,
                    });
                }
            }
        }

        for task in self.tasks.values_mut() {
            if task.status == ForgeStatus::Running && !seen.contains(&task.id) {
                task.status = ForgeStatus::Done;
                task.current_tool = None;
            }
        }

        if had_running && self.running_count() == 0 {
            self.folds_at = Some(Instant::now() + FORGE_FOLD);
        }
        self.clamp_selection();
    }

    /// La notification ne parle que d'outil : le statut appartient au snapshot.
    /// Elle arrive souvent avant lui, d'où la création à la volée — l'id tient
    /// lieu de description jusqu'au prochain tick.
    pub fn apply_tool_notification(&mut self, subagent_id: &str, tool_name: &str) {
        match self.tasks.get_mut(subagent_id) {
            Some(task) => task.current_tool = Some(tool_name.to_string()),
            None => self.insert(ForgeTask {
                id: subagent_id.to_string(),
                description: subagent_id.to_string(),
                status: ForgeStatus::Running,
                current_tool: Some(tool_name.to_string()),
                elapsed_secs: 0,
                turns: 0,
                result: None,
                error: None,
            }),
        }
    }

    /// Annuler la dernière tâche vive arme le repli au lieu de fermer le volet
    /// net : l'annulation est un verdict, elle se lit comme les autres.
    pub fn mark_cancelled(&mut self, id: &str) {
        let had_running = self.running_count() > 0;
        let Some(task) = self.tasks.get_mut(id) else {
            return;
        };
        task.status = ForgeStatus::Cancelled;
        task.current_tool = None;
        if had_running && self.running_count() == 0 {
            self.folds_at = Some(Instant::now() + FORGE_FOLD);
        }
    }

    pub fn visible(&self) -> bool {
        match self.view {
            ForgeView::ForcedOpen => true,
            ForgeView::ForcedClosed => false,
            ForgeView::Auto => {
                !self.tasks.is_empty()
                    && (self.running_count() > 0
                        || self.folds_at.is_some_and(|at| at > Instant::now()))
            }
        }
    }

    pub fn toggle(&mut self) {
        self.view = if self.visible() {
            ForgeView::ForcedClosed
        } else {
            ForgeView::ForcedOpen
        };
    }

    pub fn running_count(&self) -> usize {
        self.tasks
            .values()
            .filter(|task| task.status == ForgeStatus::Running)
            .count()
    }

    pub fn selected_task(&self) -> Option<&ForgeTask> {
        self.tasks.values().nth(self.selected)
    }

    /// Une lame qui vient de naître rouvre le volet, même refermé à la main :
    /// la main de l'utilisateur portait sur la fournée précédente.
    fn insert(&mut self, task: ForgeTask) {
        let running = task.status == ForgeStatus::Running;
        self.tasks.insert(task.id.clone(), task);
        if running {
            self.view = ForgeView::Auto;
            self.folds_at = None;
        }
    }

    fn clamp_selection(&mut self) {
        self.selected = self.selected.min(self.tasks.len().saturating_sub(1));
    }
}

fn reconciled_status(status: SubagentTaskStatus) -> ForgeStatus {
    match status {
        SubagentTaskStatus::Running => ForgeStatus::Running,
        SubagentTaskStatus::Completed => ForgeStatus::Done,
        SubagentTaskStatus::Failed => ForgeStatus::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(id: &str, status: SubagentTaskStatus) -> SubagentTaskSnapshot {
        SubagentTaskSnapshot {
            id: id.to_string(),
            description: format!("forge {id}"),
            status,
            turns: 2,
            elapsed_secs: 7,
            result: None,
            error: None,
        }
    }

    #[test]
    fn notification_poses_tool_without_touching_status() {
        let mut state = ForgeState::default();
        state.apply_snapshot(vec![snapshot("t1", SubagentTaskStatus::Running)]);
        state.apply_tool_notification("t1", "developer__shell");

        let task = &state.tasks["t1"];
        assert_eq!(task.current_tool.as_deref(), Some("developer__shell"));
        assert_eq!(task.status, ForgeStatus::Running);
        assert_eq!(task.description, "forge t1");
    }

    #[test]
    fn notification_before_any_snapshot_creates_the_task() {
        let mut state = ForgeState::default();
        state.apply_tool_notification("t1", "developer__shell");

        assert_eq!(state.tasks["t1"].description, "t1");
        assert_eq!(state.tasks["t1"].status, ForgeStatus::Running);

        state.apply_snapshot(vec![snapshot("t1", SubagentTaskStatus::Running)]);

        assert_eq!(state.tasks["t1"].description, "forge t1");
        assert_eq!(
            state.tasks["t1"].current_tool.as_deref(),
            Some("developer__shell")
        );
    }

    #[test]
    fn running_task_absent_from_the_snapshot_ends_done() {
        let mut state = ForgeState::default();
        state.apply_snapshot(vec![snapshot("t1", SubagentTaskStatus::Running)]);
        state.apply_tool_notification("t1", "developer__shell");
        state.apply_snapshot(vec![]);

        let task = &state.tasks["t1"];
        assert_eq!(task.status, ForgeStatus::Done);
        assert!(task.current_tool.is_none());
    }

    #[test]
    fn cancelled_task_never_reverts_to_running() {
        let mut state = ForgeState::default();
        state.apply_snapshot(vec![snapshot("t1", SubagentTaskStatus::Running)]);
        state.mark_cancelled("t1");
        state.apply_snapshot(vec![snapshot("t1", SubagentTaskStatus::Running)]);

        assert_eq!(state.tasks["t1"].status, ForgeStatus::Cancelled);
        assert_eq!(state.running_count(), 0);
    }

    #[test]
    fn cancelling_the_last_live_task_arms_the_fold() {
        let mut state = ForgeState::default();
        state.apply_snapshot(vec![snapshot("t1", SubagentTaskStatus::Running)]);

        state.mark_cancelled("t1");

        let armed = state.folds_at.expect("repli armé à l'annulation");
        assert!(state.visible(), "le volet se replie, il ne claque pas");

        state.apply_snapshot(vec![]);
        assert_eq!(state.folds_at, Some(armed));
    }

    #[test]
    fn cancelling_one_of_several_leaves_the_fold_unarmed() {
        let mut state = ForgeState::default();
        state.apply_snapshot(vec![
            snapshot("t1", SubagentTaskStatus::Running),
            snapshot("t2", SubagentTaskStatus::Running),
        ]);

        state.mark_cancelled("t1");

        assert!(state.folds_at.is_none(), "la forge tourne encore");
        assert_eq!(state.running_count(), 1);
    }

    #[test]
    fn fold_is_armed_once_when_the_last_task_stops() {
        let mut state = ForgeState::default();
        state.apply_snapshot(vec![snapshot("t1", SubagentTaskStatus::Running)]);
        assert!(state.folds_at.is_none());

        state.apply_snapshot(vec![snapshot("t1", SubagentTaskStatus::Completed)]);
        let armed = state.folds_at.expect("repli armé à la dernière tâche");

        state.apply_snapshot(vec![snapshot("t1", SubagentTaskStatus::Completed)]);
        assert_eq!(state.folds_at, Some(armed));
    }

    #[test]
    fn a_new_task_reopens_a_forced_closed_panel() {
        let mut state = ForgeState::default();
        state.apply_snapshot(vec![snapshot("t1", SubagentTaskStatus::Completed)]);
        state.view = ForgeView::ForcedClosed;
        state.folds_at = Some(Instant::now() + FORGE_FOLD);

        state.apply_snapshot(vec![
            snapshot("t1", SubagentTaskStatus::Completed),
            snapshot("t2", SubagentTaskStatus::Running),
        ]);

        assert_eq!(state.view, ForgeView::Auto);
        assert!(state.folds_at.is_none());
        assert!(state.visible());
    }

    #[test]
    fn visible_reads_the_view_then_the_activity() {
        let mut state = ForgeState::default();
        assert!(!state.visible(), "Auto sans tâche : rien à montrer");

        state.view = ForgeView::ForcedOpen;
        assert!(state.visible(), "ouvert de force, même vide");

        state.view = ForgeView::Auto;
        state.apply_snapshot(vec![snapshot("t1", SubagentTaskStatus::Running)]);
        assert!(state.visible());

        state.view = ForgeView::ForcedClosed;
        assert!(!state.visible(), "fermé de force, même en pleine forge");

        state.view = ForgeView::Auto;
        state.apply_snapshot(vec![snapshot("t1", SubagentTaskStatus::Completed)]);
        assert!(
            state.visible(),
            "le verdict reste lisible le temps du repli"
        );

        state.folds_at = Some(Instant::now() - Duration::from_secs(1));
        assert!(!state.visible(), "repli échu");
    }

    #[test]
    fn toggle_flips_between_the_two_forced_states() {
        let mut state = ForgeState {
            view: ForgeView::ForcedOpen,
            ..Default::default()
        };
        state.toggle();
        assert_eq!(state.view, ForgeView::ForcedClosed);

        state.toggle();
        assert_eq!(state.view, ForgeView::ForcedOpen);
    }

    #[test]
    fn selection_is_clamped_to_the_task_count() {
        let mut state = ForgeState::default();
        state.apply_snapshot(vec![
            snapshot("t1", SubagentTaskStatus::Running),
            snapshot("t2", SubagentTaskStatus::Running),
        ]);
        state.selected = 9;
        state.apply_snapshot(vec![
            snapshot("t1", SubagentTaskStatus::Running),
            snapshot("t2", SubagentTaskStatus::Running),
        ]);

        assert_eq!(state.selected, 1);
        assert_eq!(
            state.selected_task().map(|task| task.id.as_str()),
            Some("t2")
        );
    }
}
