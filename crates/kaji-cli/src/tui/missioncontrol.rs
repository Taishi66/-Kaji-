//! 炉 mission-control — la forge en plein écran.
//!
//! Le volet forge (32 colonnes) dit ce que les lames font ; cette vue dit
//! comment elles s'ordonnent. Une carte bordée par stage du workflow actif, ou
//! une seule carte « unattached » quand la session ne pilote aucun workflow et
//! que les lames viennent de summons isolés.
//!
//! Deux sources, deux autorités, comme dans le volet : le snapshot du workflow
//! (`WorkflowHandle::snapshot`) fait autorité sur les états et la topologie,
//! l'usage ledger sur les tokens et le coût. Rien ici n'invente une mesure — un
//! agent sans ligne au ledger affiche `炭 —`, il n'affiche pas zéro.
//!
//! Les actions se branchent sur la sélection : ⏎ ouvre la fiche, `x` annule,
//! `p` suspend un stage, `g` tranche sa gate. Chaque carte porte donc sa
//! **clé** — le nom d'agent ou l'identifiant de lame bruts — à côté du nom
//! affiché, qui est assaini et ne peut pas servir d'adresse.
//!
//! v2 (contrat 2026-09-07) : tout le texte est anglais, les kanji restent mais
//! ne portent jamais seuls un sens — chacun est collé au mot anglais qu'il
//! décore (`炉 mission-control`, `遣 details`, `門 gate`, `炭 tokens`,
//! `刻 timeline`). Un état est toujours un symbole **et** un mot.

use crate::tui::app::App;
use crate::tui::forge::{ForgeStatus, ForgeTask};
use crate::tui::ui::{forge_duration, sanitize_for_display};
use crate::tui::{gitstatus, statusbar, theme};
use kaji::workflow::{AgentState, FailureCause, StageState, WorkflowState};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use std::collections::{HashMap, HashSet};

/// La colonne des lames qui ne relèvent d'aucun stage — un summon lancé à la
/// main pendant qu'un workflow tourne ou non.
pub const FREE_STAGE: &str = "unattached";

/// Largeur d'une carte de stage, bordures comprises. La ligne d'usage
/// (`炭 12.3k↑ 4.5k↓ · $12.34 · 12m34s`) est ce qui la calibre ; le panneau de
/// détails porte la version non tronquée.
const STAGE_WIDTH: usize = 30;

/// Gouttière entre deux cartes. Deux cellules : un kanji ne doit jamais toucher
/// la carte d'à côté.
const STAGE_GAP: usize = 2;

/// Les deux bordures horizontales d'une carte de stage.
const STAGE_FRAME_ROWS: usize = 2;

/// Une sous-carte d'agent : bordure haute (nom + badge), ligne d'usage,
/// bordure basse.
const AGENT_CARD_ROWS: usize = 3;

/// Ce qu'une bordure verticale et sa respiration coûtent de chaque côté.
const FRAME_PADDING: usize = 2;

/// Sous cette largeur le panneau de détails disparaît : ⏎ garde la fiche
/// lecteur, qui est la version pleine page du même contenu.
pub const DETAILS_MIN_WIDTH: u16 = 100;

/// Largeur du panneau de détails. C'est lui qui mange l'espace que les cartes
/// laissent à droite.
const DETAILS_WIDTH: u16 = 32;

/// Hauteur minimale sous laquelle le bandeau timeline cède la place aux
/// cartes : une carte vaut mieux qu'une barre.
const TIMELINE_MIN_HEIGHT: u16 = 14;

/// Barres au bandeau, en plus de ses deux bordures. Au-delà, une ligne de reste.
const TIMELINE_MAX_BARS: usize = 5;

/// Cellules du libellé d'une barre de timeline.
const TIMELINE_LABEL_CELLS: usize = 14;

/// Cellules réservées au chrono en queue de barre — `12m34s` et sa respiration.
const TIMELINE_DURATION_CELLS: usize = 8;

/// Colonne où le panneau de détails aligne ses valeurs.
const DETAILS_LABEL_CELLS: usize = 9;

const BAR_FULL: char = '█';
const BAR_EMPTY: char = '░';

/// Le tracé d'une carte. Le lourd marque la sélection — une bordure accent
/// épaisse se voit sans couleur, donc aussi en thème `mono`.
struct Frame9 {
    top_left: char,
    top_right: char,
    bottom_left: char,
    bottom_right: char,
    horizontal: char,
    vertical: char,
}

const LIGHT_FRAME: Frame9 = Frame9 {
    top_left: '┌',
    top_right: '┐',
    bottom_left: '└',
    bottom_right: '┘',
    horizontal: '─',
    vertical: '│',
};

const HEAVY_FRAME: Frame9 = Frame9 {
    top_left: '┏',
    top_right: '┓',
    bottom_left: '┗',
    bottom_right: '┛',
    horizontal: '━',
    vertical: '┃',
};

/// Ce qu'une carte sait de la consommation d'un agent. `None` quand le ledger
/// n'a pas encore de ligne pour sa session — ou qu'aucune session ne lui est
/// attachée, ce qui est le cas des summons libres tant que
/// `SubagentTaskSnapshot` ne porte pas de `session_id`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AgentUsage {
    pub input: i64,
    pub output: i64,
    pub cost: Option<f64>,
}

/// L'état de la vue : ce qu'elle montre et où l'œil est posé. Les deux
/// curseurs sont bornés à chaque construction de plateau, jamais à l'écriture.
#[derive(Debug, Default)]
pub struct MissionState {
    pub open: bool,
    pub stage: usize,
    pub card: usize,
    /// Le snapshot du workflow que la session pilote, s'il y en a un.
    pub workflow: Option<WorkflowState>,
    /// Usage par identifiant de session d'agent.
    pub usage: HashMap<String, AgentUsage>,
    /// Les stages qu'une pause vise, `WorkflowHandle::paused_stages` faisant
    /// foi. Un stage pas encore démarré ne portera `StageState::Paused` qu'en
    /// atteignant son point d'arrêt : sans cette table, `p` reposerait une
    /// pause déjà posée au lieu de la lever, et le stage resterait suspendu
    /// sans moyen de le relâcher.
    pub paused: HashSet<String>,
    /// La dernière réponse à une action — verdict de gate, refus d'annulation,
    /// issue du run. Le plein écran cache le chat : sans elle, un `g` sur une
    /// porte fermée serait un non-événement silencieux. Elle répond à une carte
    /// précise : naviguer ou fermer la vue l'efface, et la bannière retrouve la
    /// gate qui attend.
    pub notice: Option<String>,
}

/// Le langage d'état de la vue, unique et partout le même : un symbole **et**
/// un mot anglais. Ni la couleur ni le kanji ne portent jamais seuls le sens —
/// un thème `mono` et un terminal sans couleur disent la même chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardMark {
    Pending,
    Running,
    Gate,
    Paused,
    Done,
    Failed,
    Cancelled,
}

impl CardMark {
    pub fn symbol(self) -> &'static str {
        match self {
            CardMark::Pending => "○",
            CardMark::Running => "●",
            CardMark::Gate => "◔",
            CardMark::Paused => "‖",
            CardMark::Done => "✓",
            CardMark::Failed => "✗",
            CardMark::Cancelled => "⊘",
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            CardMark::Pending => "pending",
            CardMark::Running => "running",
            CardMark::Gate => "gate",
            CardMark::Paused => "paused",
            CardMark::Done => "done",
            CardMark::Failed => "failed",
            CardMark::Cancelled => "cancelled",
        }
    }

    /// Le badge affiché : jamais le symbole seul, jamais le mot seul.
    pub fn badge(self) -> String {
        format!("{} {}", self.symbol(), self.word())
    }

    /// La couleur ne fait que redoubler le badge. `Gate` et `Paused` partagent
    /// l'or — ce qui attend une main humaine ; le gras distingue la porte, qui
    /// bloque, de la pause, qui a été voulue.
    pub fn style(self) -> Style {
        match self {
            CardMark::Pending => theme::dim(),
            CardMark::Running => theme::accent(),
            CardMark::Gate => theme::warning().add_modifier(Modifier::BOLD),
            CardMark::Paused => theme::warning(),
            CardMark::Done => theme::success(),
            CardMark::Failed => theme::accent().add_modifier(Modifier::BOLD),
            CardMark::Cancelled => theme::dim().add_modifier(Modifier::DIM),
        }
    }

    /// Ce qui prime quand un plateau entier se résume à un seul badge : la
    /// porte qui attend passe devant l'échec, qui passe devant ce qui tourne.
    /// Sans cette précédence, un header dirait « running » d'un workflow que
    /// plus rien ne fera avancer sans une décision humaine.
    fn rank(self) -> u8 {
        match self {
            CardMark::Gate => 0,
            CardMark::Failed => 1,
            CardMark::Running => 2,
            CardMark::Paused => 3,
            CardMark::Pending => 4,
            CardMark::Cancelled => 5,
            CardMark::Done => 6,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Card {
    /// Le nom d'agent ou l'identifiant de lame **bruts** — ce que les actions
    /// adressent. [`Card::name`] passe par `sanitize_for_display` et par une
    /// troncature en cellules : il désigne à l'œil, jamais à l'exécuteur.
    pub key: String,
    pub name: String,
    pub mark: CardMark,
    pub tool: Option<String>,
    pub usage: Option<AgentUsage>,
    pub elapsed_secs: u64,
    /// La session d'agent, quand elle existe — le panneau de détails en fait
    /// l'adresse d'un rejeu.
    pub session: Option<String>,
    /// La cause d'un échec, telle que l'exécuteur l'a nommée.
    pub detail: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub mark: CardMark,
    /// Le badge du stage, symbole et mot — ce que la bordure haute affiche.
    pub state: String,
    /// Le nom brut du stage, `None` pour la colonne des lames libres — c'est
    /// ce qui distingue une carte d'agent de workflow d'un summon isolé.
    pub stage: Option<String>,
    pub cards: Vec<Card>,
}

impl Column {
    /// La durée du stage : ses agents tournent en parallèle, donc c'est le plus
    /// long qui dit combien de temps le stage a pris, pas leur somme.
    fn elapsed_secs(&self) -> u64 {
        self.cards
            .iter()
            .map(|card| card.elapsed_secs)
            .max()
            .unwrap_or(0)
    }
}

#[derive(Debug, Clone)]
pub struct Board {
    pub title: String,
    pub columns: Vec<Column>,
}

impl Board {
    fn cards(&self) -> impl Iterator<Item = &Card> {
        self.columns.iter().flat_map(|column| column.cards.iter())
    }

    fn is_empty(&self) -> bool {
        self.cards().next().is_none()
    }

    /// Les stages dont la porte attend une décision, dans l'ordre du document :
    /// la plus ancienne d'abord, c'est celle que la bannière nomme.
    fn waiting_gates(&self) -> Vec<&str> {
        self.columns
            .iter()
            .filter(|column| column.mark == CardMark::Gate)
            .filter_map(|column| column.stage.as_deref())
            .collect()
    }
}

/// Ce que la carte sous le curseur désigne. Les touches d'action ne consomment
/// que ça : un agent d'un stage se pilote par le `WorkflowHandle`, une lame
/// libre par le summon, et les deux chemins ne se confondent jamais.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MissionTarget {
    Agent { stage: String, agent: String },
    Blade { id: String },
}

/// La cible d'un couple de curseurs déjà bornés. `None` sur une colonne vide —
/// le plateau libre d'une session au repos en est une.
pub fn target(board: &Board, column: usize, card: usize) -> Option<MissionTarget> {
    let column = board.columns.get(column)?;
    let key = column.cards.get(card)?.key.clone();
    Some(match column.stage.as_ref() {
        Some(stage) => MissionTarget::Agent {
            stage: stage.clone(),
            agent: key,
        },
        None => MissionTarget::Blade { id: key },
    })
}

/// Le plateau que la vue rend : les stages du workflow piloté, puis les lames
/// du volet forge. Sans workflow il ne reste que la colonne « unattached »,
/// rendue même vide — une vue ouverte sur rien doit dire qu'elle n'a rien.
///
/// La colonne libre n'est pas filtrée : le `SubagentRunner` du workflow appelle
/// `run_subagent_task` en direct, et seul l'outil `delegate` inscrit une lame
/// dans les `background_tasks` de summon — un agent de workflow n'y apparaît
/// donc jamais. Si l'exécuteur passe un jour par summon, il faudra réintroduire
/// un filtre ici : la session d'un agent de workflow **est** l'identifiant de
/// sa lame côté summon, et l'agent aurait alors deux cartes.
pub fn board(app: &App) -> Board {
    let tasks = app.forge.ordered();
    match app.mission.workflow.as_ref() {
        Some(workflow) => {
            let mut board = workflow_board(workflow, &app.mission.usage, &app.mission.paused);
            if !tasks.is_empty() {
                board.columns.push(free_column(&tasks, &app.mission.usage));
            }
            board
        }
        None => free_board(&tasks, &app.mission.usage),
    }
}

/// L'état affiché d'un stage. Une pause posée sur un stage pas encore démarré
/// ne se lit nulle part dans son `StageState` : sans cette ligne, la vue
/// afficherait « pending » d'un stage que plus rien ne fera partir.
fn stage_mark(stage: &kaji::workflow::StageStatus, paused: &HashSet<String>) -> CardMark {
    if stage.state != StageState::Paused
        && !stage.state.is_terminal()
        && paused.contains(&stage.name)
    {
        return CardMark::Paused;
    }
    match stage.state {
        StageState::Pending => CardMark::Pending,
        StageState::Running => CardMark::Running,
        StageState::Waiting => CardMark::Gate,
        StageState::Paused => CardMark::Paused,
        StageState::Done => CardMark::Done,
        StageState::Failed(_) => CardMark::Failed,
        StageState::Cancelled => CardMark::Cancelled,
    }
}

fn workflow_board(
    workflow: &WorkflowState,
    usage: &HashMap<String, AgentUsage>,
    paused: &HashSet<String>,
) -> Board {
    Board {
        title: sanitize_for_display(&workflow.workflow),
        columns: workflow
            .stages
            .iter()
            .map(|stage| {
                let mark = stage_mark(stage, paused);
                Column {
                    name: sanitize_for_display(&stage.name),
                    mark,
                    state: mark.badge(),
                    stage: Some(stage.name.clone()),
                    cards: stage
                        .agents
                        .iter()
                        .map(|agent| Card {
                            key: agent.name.clone(),
                            name: sanitize_for_display(&agent.name),
                            mark: agent_mark(&agent.state, &stage.state),
                            tool: None,
                            usage: agent
                                .session_id
                                .as_deref()
                                .and_then(|id| usage.get(id).copied()),
                            elapsed_secs: (agent.duration_ms.max(0) / 1000) as u64,
                            session: agent.session_id.clone(),
                            detail: failure_detail(&agent.state),
                        })
                        .collect(),
                }
            })
            .collect(),
    }
}

fn free_board(tasks: &[&ForgeTask], usage: &HashMap<String, AgentUsage>) -> Board {
    Board {
        title: FREE_STAGE.to_string(),
        columns: vec![free_column(tasks, usage)],
    }
}

fn free_column(tasks: &[&ForgeTask], usage: &HashMap<String, AgentUsage>) -> Column {
    let mark = free_column_mark(tasks);
    Column {
        name: FREE_STAGE.to_string(),
        mark,
        state: mark.badge(),
        stage: None,
        cards: tasks
            .iter()
            .map(|task| Card {
                key: task.id.clone(),
                name: sanitize_for_display(&task.description.replace('\n', "␊")),
                mark: forge_mark(task.status),
                tool: task.current_tool.clone(),
                usage: usage.get(&task.id).copied(),
                elapsed_secs: task.elapsed_secs,
                session: None,
                detail: task.error.clone(),
            })
            .collect(),
    }
}

/// Le badge de la colonne libre est celui de ses lames : elle tourne tant
/// qu'une lame tourne, elle a fini quand toutes ont fini.
fn free_column_mark(tasks: &[&ForgeTask]) -> CardMark {
    tasks
        .iter()
        .map(|task| forge_mark(task.status))
        .min_by_key(|mark| mark.rank())
        .unwrap_or(CardMark::Pending)
}

/// L'état du stage l'emporte sur celui de l'agent tant que l'agent n'a pas
/// commencé : une porte ouverte ou une pause décrit ce qui bloque, là où
/// « pending » ne dirait pas pourquoi.
fn agent_mark(agent: &AgentState, stage: &StageState) -> CardMark {
    match agent {
        AgentState::Running => CardMark::Running,
        AgentState::Done => CardMark::Done,
        AgentState::Failed(_) => CardMark::Failed,
        AgentState::Cancelled => CardMark::Cancelled,
        AgentState::Pending => match stage {
            StageState::Waiting => CardMark::Gate,
            StageState::Paused => CardMark::Paused,
            _ => CardMark::Pending,
        },
    }
}

/// Ce que l'exécuteur a nommé comme cause — le panneau de détails la rend, la
/// vue ne la reformule pas.
fn failure_detail(state: &AgentState) -> Option<String> {
    match state {
        AgentState::Failed(FailureCause::Budget(limit)) => {
            Some(format!("budget {} exceeded", limit.field()))
        }
        AgentState::Failed(FailureCause::Error(error)) => Some(error.clone()),
        _ => None,
    }
}

fn forge_mark(status: ForgeStatus) -> CardMark {
    match status {
        ForgeStatus::Running => CardMark::Running,
        ForgeStatus::Done => CardMark::Done,
        ForgeStatus::Failed => CardMark::Failed,
        ForgeStatus::Cancelled => CardMark::Cancelled,
    }
}

/// Combien de cartes de stage tiennent dans `width` cellules. Au moins une :
/// sous la largeur d'une carte, elle se rétrécit plutôt que la vue ne
/// disparaisse.
pub fn visible_columns(width: usize, columns: usize) -> usize {
    if columns == 0 {
        return 0;
    }
    let fitting = (width + STAGE_GAP) / (STAGE_WIDTH + STAGE_GAP);
    fitting.max(1).min(columns)
}

/// Le premier élément rendu d'une fenêtre de `visible` emplacements sur
/// `total` : elle glisse pour tenir `selected` visible, sans jamais le
/// pousser en tête tant qu'il reste de la place derrière lui. Partagée entre
/// les cartes de stages et les sous-cartes d'agents — même contrat que le
/// volet forge et l'explorateur.
fn sliding_window_start(selected: usize, visible: usize, total: usize) -> usize {
    if visible == 0 || total <= visible {
        return 0;
    }
    let last = total - visible;
    selected.saturating_sub(visible.saturating_sub(1)).min(last)
}

/// La première carte de stage rendue.
pub fn first_column(selected: usize, visible: usize, columns: usize) -> usize {
    sliding_window_start(selected, visible, columns)
}

/// Le panneau de détails ne s'ouvre qu'au-dessus de [`DETAILS_MIN_WIDTH`] :
/// plus étroit, il volerait aux cartes la largeur qui les rend lisibles, et ⏎
/// donne déjà la même fiche en pleine page.
pub fn details_width(width: u16) -> u16 {
    if width < DETAILS_MIN_WIDTH {
        return 0;
    }
    DETAILS_WIDTH
}

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    frame.render_widget(Clear, area);

    let board = board(app);
    let banner = banner_text(&board, app.mission.notice.as_deref());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::border_active())
        .title(Span::styled(
            header_title(&board, area.width),
            theme::title(),
        ))
        .title_top(Line::from(Span::styled(header_summary(&board), theme::dim())).right_aligned())
        .title_bottom(
            Line::from(footer_keys(
                &board,
                app.mission.stage,
                app.mission.card,
                area.width,
            ))
            .style(theme::dim()),
        );

    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let banner_rows = u16::from(banner.is_some() && inner.height > 1);
    if let Some(banner) = banner.filter(|_| banner_rows > 0) {
        frame.render_widget(
            Paragraph::new(Line::from(pad_cells(&banner, usize::from(inner.width))))
                .style(theme::accent().add_modifier(Modifier::REVERSED)),
            Rect { height: 1, ..inner },
        );
    }

    let body = Rect {
        y: inner.y + banner_rows,
        height: inner.height.saturating_sub(banner_rows),
        ..inner
    };
    if body.height == 0 {
        return;
    }

    let details = details_width(area.width).min(body.width.saturating_sub(1));
    let left = Rect {
        width: body.width - details,
        ..body
    };
    if details > 0 {
        draw_details(
            frame,
            &board,
            app.mission.stage,
            app.mission.card,
            Rect {
                x: body.x + left.width,
                width: details,
                ..body
            },
        );
    }

    if board.is_empty() && app.mission.workflow.is_none() {
        frame.render_widget(
            Paragraph::new(Text::from(empty_state_lines(usize::from(left.width)))),
            left,
        );
        return;
    }

    let bars = timeline_rows(&board, left.height);
    let cards_height = left.height.saturating_sub(bars);
    draw_stage_cards(
        frame,
        app,
        &board,
        Rect {
            height: cards_height,
            ..left
        },
    );
    if bars > 0 {
        draw_timeline(
            frame,
            &board,
            Rect {
                y: left.y + cards_height,
                height: bars,
                ..left
            },
        );
    }
}

/// Le titre de gauche : le nom du workflow et, quand des stages sortent du
/// champ, combien. Sans ce compteur une vue étroite ferait croire que le
/// workflow n'a que les cartes visibles.
pub fn header_title(board: &Board, width: u16) -> String {
    format!(
        " {} mission-control · {}{} ",
        theme::FORGE_GLYPH,
        board.title,
        hidden_marker(board, 0, width)
    )
}

/// Le résumé de droite : l'état d'ensemble, le compte de stages, ce que le run
/// a brûlé et depuis combien de temps. Un plateau sans ligne au ledger dit
/// `炭 —`, jamais zéro.
pub fn header_summary(board: &Board) -> String {
    let mark = board
        .columns
        .iter()
        .map(|column| column.mark)
        .min_by_key(|mark| mark.rank())
        .unwrap_or(CardMark::Pending);
    let stages = board
        .columns
        .iter()
        .filter(|column| column.stage.is_some())
        .count();
    let scope = match stages {
        0 => {
            let tasks = board.cards().count();
            format!("{tasks} task{}", plural(tasks))
        }
        1 => "1 stage".to_string(),
        many => format!("{many} stages"),
    };
    let elapsed = board
        .columns
        .iter()
        .map(Column::elapsed_secs)
        .max()
        .unwrap_or(0);

    format!(
        " {} · {scope} · {} · {} ",
        mark.badge(),
        aggregate_usage(board),
        forge_duration(elapsed)
    )
}

fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

/// La somme du ledger sur tout le plateau. Le coût ne s'affiche que si au moins
/// une ligne en porte un : additionner des `None` en zéro ferait passer un run
/// non tarifé pour un run gratuit.
fn aggregate_usage(board: &Board) -> String {
    let rows: Vec<AgentUsage> = board.cards().filter_map(|card| card.usage).collect();
    if rows.is_empty() {
        return format!("{} —", theme::TOKENS_GLYPH);
    }
    let input: i64 = rows.iter().map(|usage| usage.input).sum();
    let output: i64 = rows.iter().map(|usage| usage.output).sum();
    let cost: Option<f64> = rows
        .iter()
        .filter_map(|usage| usage.cost)
        .map(Some)
        .reduce(|total, cost| Some(total.unwrap_or(0.0) + cost.unwrap_or(0.0)))
        .flatten();
    format!(
        "{} {}↑ {}↓ · {}",
        theme::TOKENS_GLYPH,
        statusbar::compact_count(input),
        statusbar::compact_count(output),
        match cost {
            Some(cost) => format!("${cost:.2}"),
            None => "$—".to_string(),
        }
    )
}

/// L'anti-confusion n°1 : dès qu'une porte attend, une ligne pleine largeur le
/// dit et nomme la touche. La réponse à une action prend sa place le temps de
/// sa péremption — c'est la seule chose plus fraîche qu'une gate ouverte, et
/// le badge du stage continue de porter la porte pendant ce temps.
pub fn banner_text(board: &Board, notice: Option<&str>) -> Option<String> {
    if let Some(notice) = notice {
        // Une notice porte des noms venus de la spec : `sanitize_for_display`
        // laisse passer `\n`, qui casserait cette ligne unique.
        return Some(format!(
            " {} ",
            sanitize_for_display(&notice.replace('\n', "␊"))
        ));
    }
    let gates = board.waiting_gates();
    let first = gates.first()?;
    let more = match gates.len() {
        1 => String::new(),
        many => format!(" (+{} more)", many - 1),
    };
    Some(format!(
        " ▶ {} gate \"{}\" is waiting{more} — press g to decide ",
        theme::GATE_GLYPH,
        sanitize_for_display(&first.replace('\n', "␊"))
    ))
}

/// Les touches, toujours visibles et contextuelles : pas de `g` sans porte
/// ouverte, pas de `p` sur une lame libre. Un pied qui promet une action qui ne
/// se produit pas est pire que pas de pied du tout.
pub fn footer_keys(board: &Board, stage: usize, card: usize, width: u16) -> String {
    let column = board.columns.get(stage);
    let mut keys = vec!["⏎ details"];
    if column.is_some_and(|column| column.mark == CardMark::Gate) {
        keys.push("g gate");
    }
    if column.is_some_and(|column| column.stage.is_some()) {
        keys.push("p pause");
    }
    if column.is_some_and(|column| column.cards.get(card).is_some()) {
        keys.push("x cancel");
    }
    keys.push("h/l stage");
    keys.push("j/k agent");
    keys.push("Esc close");

    let full = format!(" {} ", keys.join(" · "));
    let budget = usize::from(width.saturating_sub(2));
    if gitstatus::display_width(&full) <= budget {
        return full;
    }
    let short = " ⏎ g p x · h/l j/k · Esc ";
    if gitstatus::display_width(short) <= budget {
        return short.to_string();
    }
    " Esc ".to_string()
}

/// Ce que la vue dit quand elle n'a rien à montrer — la seule sortie où le
/// plateau doit expliquer où trouver du travail plutôt que d'afficher un cadre
/// vide.
pub fn empty_state_lines(width: usize) -> Vec<Line<'static>> {
    let lines = [
        (
            format!(
                "{} mission-control — no workflow running",
                theme::FORGE_GLYPH
            ),
            theme::title(),
        ),
        (String::new(), theme::dim()),
        (
            "start one with /workflow <file.yaml>".to_string(),
            theme::dim(),
        ),
        (
            "free-agent tasks appear in the forge panel (Ctrl+F)".to_string(),
            theme::dim(),
        ),
    ];
    lines
        .into_iter()
        .map(|(text, style)| {
            Line::from(Span::styled(gitstatus::truncate_cells(&text, width), style))
        })
        .collect()
}

fn draw_stage_cards(frame: &mut Frame, app: &App, board: &Board, area: Rect) {
    if area.height == 0 || area.width == 0 || board.columns.is_empty() {
        return;
    }
    let width = usize::from(area.width);
    let visible = visible_columns(width, board.columns.len());
    let first = first_column(app.mission.stage, visible, board.columns.len());
    // Une largeur entière par carte, gouttière comprise : un reste partagé en
    // pourcentage ferait déborder la dernière d'une cellule sur un écran
    // impair, et le `Paragraph` la rognerait sans le dire. Plafonnée à la
    // largeur calibrée : sur un écran très large, étirer trois cartes à
    // soixante cellules éparpillerait ce qui en tient trente.
    let slot = (width / visible).min(STAGE_WIDTH + STAGE_GAP);

    for (slot_index, column_index) in (first..first + visible).enumerate() {
        let Some(column) = board.columns.get(column_index) else {
            break;
        };
        let x = area.x + (slot_index * slot) as u16;
        let card_width = slot.saturating_sub(STAGE_GAP).max(1);
        let selected = (column_index == app.mission.stage).then_some(app.mission.card);
        frame.render_widget(
            Paragraph::new(Text::from(stage_card_lines(
                column,
                card_width,
                usize::from(area.height),
                selected,
            ))),
            Rect {
                x,
                y: area.y,
                width: card_width as u16,
                height: area.height,
            },
        );
    }
}

/// Une carte de stage : sa bordure porte le nom et le badge, son ventre porte
/// une sous-carte par agent. La bordure passe en accent épais quand le stage
/// est sélectionné — la sélection se voit sans couleur.
pub fn stage_card_lines(
    column: &Column,
    width: usize,
    rows: usize,
    selected: Option<usize>,
) -> Vec<Line<'static>> {
    if width < 4 || rows < 2 {
        return Vec::new();
    }
    let frame = if selected.is_some() {
        &HEAVY_FRAME
    } else {
        &LIGHT_FRAME
    };
    let border = if selected.is_some() {
        theme::border_active()
    } else {
        theme::border_inactive()
    };

    let mut lines = vec![framed_top(
        frame,
        width,
        &column.name,
        &column.state,
        1,
        border,
        column.mark.style(),
    )];

    let inner_width = width - FRAME_PADDING * 2;
    let slots = (rows - STAGE_FRAME_ROWS) / AGENT_CARD_ROWS;
    let mut body: Vec<Line<'static>> = Vec::new();
    if slots == 0 || inner_width == 0 {
        // Rien ne tient : la carte reste fermée plutôt que d'ouvrir un cadre
        // sur des sous-cartes rognées à mi-hauteur.
    } else if column.cards.is_empty() {
        body.push(framed_body(
            frame,
            width,
            &Line::from(Span::styled("no agents", theme::dim())),
            border,
        ));
    } else {
        let cursor = selected.unwrap_or(0);
        let first = sliding_window_start(cursor, slots, column.cards.len());
        for (rank, card) in column.cards.iter().enumerate().skip(first).take(slots) {
            for line in agent_card_lines(card, inner_width, Some(rank) == selected) {
                body.push(framed_body(frame, width, &line, border));
            }
        }
    }

    // La carte se ferme sur ses agents plutôt que de s'étirer sur toute la
    // hauteur : un cadre à moitié vide fait chercher un contenu qui n'existe
    // pas, et c'est le panneau de détails qui occupe l'espace rendu.
    body.truncate(rows - STAGE_FRAME_ROWS);
    lines.extend(body);
    lines.push(framed_bottom(frame, width, border));
    lines
}

/// Une sous-carte d'agent : bordure haute `遣 nom … badge`, ligne d'usage,
/// bordure basse. Deux lignes d'information, trois lignes d'écran.
pub fn agent_card_lines(card: &Card, width: usize, selected: bool) -> Vec<Line<'static>> {
    if width < 4 {
        return Vec::new();
    }
    let frame = if selected { &HEAVY_FRAME } else { &LIGHT_FRAME };
    let border = if selected {
        theme::border_active()
    } else {
        theme::border_inactive()
    };
    let title = format!("{} {}", theme::SUBAGENT_GLYPH, card.name);
    vec![
        framed_top(
            frame,
            width,
            &title,
            &card.mark.badge(),
            0,
            border,
            card.mark.style(),
        ),
        framed_body(
            frame,
            width,
            &Line::from(Span::styled(usage_line(card), theme::dim())),
            border,
        ),
        framed_bottom(frame, width, border),
    ]
}

/// `┌─ titre ──── badge ─┐`, exactement `width` cellules. Le titre cède avant
/// le badge : un état tronqué ne dit plus rien, un nom tronqué désigne encore.
///
/// `edge` est le tiret qui borde les coins — la carte de stage le porte, la
/// sous-carte d'agent non : deux cellules de moins, et c'est ce qui laisse un
/// nom d'agent entier là où le badge en prend déjà neuf.
fn framed_top(
    frame: &Frame9,
    width: usize,
    title: &str,
    badge: &str,
    edge: usize,
    border: Style,
    badge_style: Style,
) -> Line<'static> {
    let bare = || {
        Line::from(Span::styled(
            format!(
                "{}{}{}",
                frame.top_left,
                frame.horizontal.to_string().repeat(width.saturating_sub(2)),
                frame.top_right
            ),
            border,
        ))
    };

    let badge_cells = gitstatus::display_width(badge);
    // `┌` + edge + ` ` + titre + ` ` + remplissage + ` ` + badge + ` ` + edge + `┐`
    let framing = 4 + 2 * edge;
    if width < framing + badge_cells {
        return bare();
    }
    let room = width - framing - badge_cells;
    let title = gitstatus::truncate_cells(title, room.saturating_sub(2));
    let dash = frame.horizontal.to_string();
    let (head, title_cells) = if title.is_empty() {
        (format!("{}{}", frame.top_left, dash.repeat(edge)), 0)
    } else {
        let cells = gitstatus::display_width(&title) + 2;
        (
            format!("{}{} {title} ", frame.top_left, dash.repeat(edge)),
            cells,
        )
    };
    let fill = room - title_cells;

    Line::from(vec![
        Span::styled(head, border),
        Span::styled(dash.repeat(fill), border),
        Span::styled(format!(" {badge} "), badge_style),
        Span::styled(format!("{}{}", dash.repeat(edge), frame.top_right), border),
    ])
}

/// `│ contenu … │`, exactement `width` cellules — le contenu est tronqué et
/// complété en cellules, jamais en chars.
fn framed_body(
    frame: &Frame9,
    width: usize,
    content: &Line<'static>,
    border: Style,
) -> Line<'static> {
    let budget = width.saturating_sub(FRAME_PADDING * 2);
    let mut spans = vec![Span::styled(format!("{} ", frame.vertical), border)];
    let mut used = 0usize;
    for span in &content.spans {
        if used >= budget {
            break;
        }
        let text = gitstatus::truncate_cells(&span.content, budget - used);
        used += gitstatus::display_width(&text);
        spans.push(Span::styled(text, span.style));
    }
    spans.push(Span::raw(" ".repeat(budget - used.min(budget))));
    spans.push(Span::styled(format!(" {}", frame.vertical), border));
    Line::from(spans)
}

fn framed_bottom(frame: &Frame9, width: usize, border: Style) -> Line<'static> {
    Line::from(Span::styled(
        format!(
            "{}{}{}",
            frame.bottom_left,
            frame.horizontal.to_string().repeat(width.saturating_sub(2)),
            frame.bottom_right
        ),
        border,
    ))
}

fn pad_cells(text: &str, width: usize) -> String {
    let text = gitstatus::truncate_cells(text, width);
    let pad = width.saturating_sub(gitstatus::display_width(&text));
    format!("{text}{}", " ".repeat(pad))
}

/// `炭 in↑ out↓ · $coût · durée`. Un agent sans ligne au ledger affiche `—`,
/// jamais un zéro : rien ne distinguerait un agent muet d'un agent gratuit.
fn usage_line(card: &Card) -> String {
    let duration = forge_duration(card.elapsed_secs);
    match card.usage {
        Some(usage) => {
            let cost = match usage.cost {
                Some(cost) => format!("${cost:.2}"),
                None => "$—".to_string(),
            };
            format!(
                "{} {}↑ {}↓ · {cost} · {duration}",
                theme::TOKENS_GLYPH,
                statusbar::compact_count(usage.input),
                statusbar::compact_count(usage.output)
            )
        }
        None => format!("{} — · {duration}", theme::TOKENS_GLYPH),
    }
}

fn draw_details(frame: &mut Frame, board: &Board, stage: usize, card: usize, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::border_inactive())
        .title(Span::styled(
            format!(" {} details ", theme::SUBAGENT_GLYPH),
            theme::title(),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(Text::from(details_lines(
            board,
            stage,
            card,
            usize::from(inner.width),
        ))),
        inner,
    );
}

/// La carte sélectionnée en clair. C'est ce panneau qui mange l'espace que les
/// cartes laissent à droite — et la seule surface où un nom de session, un
/// outil ou une cause d'échec tiennent sans troncature agressive.
pub fn details_lines(board: &Board, stage: usize, card: usize, width: usize) -> Vec<Line<'static>> {
    let Some(column) = board.columns.get(stage) else {
        return vec![Line::from(Span::styled("no selection", theme::dim()))];
    };
    let Some(card) = column.cards.get(card) else {
        return vec![
            Line::from(Span::styled(
                gitstatus::truncate_cells(&column.name, width),
                theme::title(),
            )),
            Line::from(Span::styled(column.state.clone(), column.mark.style())),
            Line::from(String::new()),
            Line::from(Span::styled("no agent on this stage", theme::dim())),
        ];
    };

    let mut lines = vec![
        Line::from(Span::styled(
            gitstatus::truncate_cells(&card.name, width),
            theme::title(),
        )),
        Line::from(String::new()),
    ];
    lines.push(detail_field(
        "status",
        &card.mark.badge(),
        width,
        card.mark.style(),
    ));
    lines.push(detail_field("stage", &column.name, width, theme::text()));
    let (tokens, cost) = match card.usage {
        Some(usage) => (
            format!(
                "{}↑ {}↓",
                statusbar::compact_count(usage.input),
                statusbar::compact_count(usage.output)
            ),
            match usage.cost {
                Some(cost) => format!("${cost:.2}"),
                None => "$—".to_string(),
            },
        ),
        None => ("—".to_string(), "$—".to_string()),
    };
    lines.push(detail_field(
        &format!("{} tokens", theme::TOKENS_GLYPH),
        &tokens,
        width,
        theme::text(),
    ));
    lines.push(detail_field(
        "cost/dur",
        &format!("{cost} · {}", forge_duration(card.elapsed_secs)),
        width,
        theme::text(),
    ));
    lines.push(detail_field(
        "session",
        card.session.as_deref().unwrap_or("—"),
        width,
        theme::text(),
    ));
    lines.push(detail_field("recipe", &board.title, width, theme::text()));
    if let Some(tool) = card.tool.as_deref() {
        lines.push(detail_field(
            &format!("{} tool", theme::FIRE_GLYPH),
            tool,
            width,
            theme::accent(),
        ));
    }
    if let Some(detail) = card.detail.as_deref() {
        lines.push(Line::from(String::new()));
        lines.push(Line::from(Span::styled(
            gitstatus::truncate_cells(&sanitize_for_display(&detail.replace('\n', "␊")), width),
            theme::error(),
        )));
    }
    lines
}

fn detail_field(label: &str, value: &str, width: usize, style: Style) -> Line<'static> {
    let label = gitstatus::truncate_cells(label, DETAILS_LABEL_CELLS);
    let pad = DETAILS_LABEL_CELLS.saturating_sub(gitstatus::display_width(&label)) + 1;
    let budget = width.saturating_sub(DETAILS_LABEL_CELLS + 1);
    Line::from(vec![
        Span::styled(format!("{label}{}", " ".repeat(pad)), theme::dim()),
        Span::styled(
            gitstatus::truncate_cells(&sanitize_for_display(value), budget),
            style,
        ),
    ])
}

/// Hauteur du bandeau : ses deux bordures plus une barre par stage, plafonnée.
/// Zéro sur un terminal trop court — les cartes passent avant.
fn timeline_rows(board: &Board, height: u16) -> u16 {
    if height < TIMELINE_MIN_HEIGHT || board.columns.is_empty() {
        return 0;
    }
    let stages = board.columns.len();
    let bars = stages.min(TIMELINE_MAX_BARS);
    let overflow = usize::from(stages > TIMELINE_MAX_BARS);
    (STAGE_FRAME_ROWS + bars + overflow) as u16
}

fn draw_timeline(frame: &mut Frame, board: &Board, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::border_inactive())
        .title(Span::styled(
            format!(" {} timeline ", theme::ELAPSED_GLYPH),
            theme::title(),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(Text::from(timeline_lines(
            board,
            usize::from(inner.width),
            usize::from(inner.height),
        ))),
        inner,
    );
}

/// Une barre par stage, proportionnelle au plus long. Un stage qui court porte
/// une lame animée en tête de barre ; un stage qui n'a pas démarré n'a pas de
/// barre du tout — un tiret et `0s`, parce qu'une barre vide se lirait comme
/// une durée nulle mesurée.
pub fn timeline_lines(board: &Board, width: usize, rows: usize) -> Vec<Line<'static>> {
    if rows == 0 {
        return Vec::new();
    }
    let longest = board
        .columns
        .iter()
        .map(Column::elapsed_secs)
        .max()
        .unwrap_or(0);
    let bar_cells = width
        .saturating_sub(TIMELINE_LABEL_CELLS + 1 + TIMELINE_DURATION_CELLS)
        .max(1);
    // La ligne de reste se prélève sur les barres : un bandeau qui la
    // rajouterait par-dessus déborderait de la hauteur qu'on lui a donnée, et
    // le `Paragraph` mangerait la dernière barre sans le dire.
    let shown = if board.columns.len() > rows {
        rows.saturating_sub(1)
    } else {
        board.columns.len()
    };

    let mut lines = Vec::new();
    for column in board.columns.iter().take(shown) {
        let elapsed = column.elapsed_secs();
        let label = gitstatus::truncate_cells(&column.name, TIMELINE_LABEL_CELLS);
        let pad = TIMELINE_LABEL_CELLS.saturating_sub(gitstatus::display_width(&label));
        let mut spans = vec![Span::styled(
            format!("{label}{} ", " ".repeat(pad)),
            theme::dim(),
        )];

        if column.mark == CardMark::Pending {
            spans.push(Span::styled(
                format!("{}{}", "─", " ".repeat(bar_cells - 1)),
                theme::dim(),
            ));
        } else {
            let filled = if longest == 0 {
                0
            } else {
                // Arrondi au plus proche : une barre plancher rend deux durées
                // voisines identiques là où la moitié d'une cellule les sépare.
                (((elapsed as u128 * bar_cells as u128 * 2 + longest as u128)
                    / (longest as u128 * 2)) as usize)
                    .min(bar_cells)
            };
            let blade = usize::from(column.mark == CardMark::Running && filled > 0);
            spans.push(Span::styled(
                BAR_FULL.to_string().repeat(filled - blade),
                column.mark.style(),
            ));
            if blade == 1 {
                spans.push(Span::styled(
                    theme::blade_frame(std::time::Duration::from_secs(elapsed)).to_string(),
                    theme::accent().add_modifier(Modifier::BOLD),
                ));
            }
            spans.push(Span::styled(
                BAR_EMPTY.to_string().repeat(bar_cells - filled),
                theme::dim(),
            ));
        }
        spans.push(Span::styled(
            pad_cells(
                &format!(" {}", forge_duration(elapsed)),
                TIMELINE_DURATION_CELLS,
            ),
            theme::dim(),
        ));
        lines.push(Line::from(spans));
    }
    if board.columns.len() > shown {
        lines.push(Line::from(Span::styled(
            format!("… +{}", board.columns.len() - shown),
            theme::dim(),
        )));
    }
    lines
}

/// Ce que le titre ajoute quand des stages sont hors champ : sans lui, une
/// vue étroite ferait croire que le workflow n'a que les cartes visibles.
fn hidden_marker(board: &Board, selected: usize, width: u16) -> String {
    let inner =
        usize::from(width.saturating_sub(2)).saturating_sub(usize::from(details_width(width)));
    let visible = visible_columns(inner, board.columns.len());
    if visible >= board.columns.len() {
        return String::new();
    }
    let first = first_column(selected, visible, board.columns.len());
    let before = first;
    let after = board.columns.len() - first - visible;
    match (before, after) {
        (0, after) => format!(" · {after}›"),
        (before, 0) => format!(" · ‹{before}"),
        (before, after) => format!(" · ‹{before} {after}›"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaji::workflow::{AgentStatus, StageStatus};
    use kaji_core::workflow::Gate;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn agent(name: &str, state: AgentState) -> AgentStatus {
        AgentStatus {
            name: name.to_string(),
            state,
            session_id: Some(format!("sess-{name}")),
            tokens: 0,
            duration_ms: 12_000,
        }
    }

    fn stage(name: &str, state: StageState, agents: Vec<AgentStatus>) -> StageStatus {
        StageStatus {
            name: name.to_string(),
            state,
            gate: Gate::Auto,
            agents,
        }
    }

    fn workflow(stages: Vec<StageStatus>) -> WorkflowState {
        WorkflowState {
            workflow: "review".to_string(),
            stages,
        }
    }

    fn app_with(workflow: WorkflowState) -> App {
        let mut app = App::new(None);
        app.mission.workflow = Some(workflow);
        app.mission.open = true;
        app
    }

    fn card(name: &str, mark: CardMark, elapsed_secs: u64) -> Card {
        Card {
            key: name.to_string(),
            name: name.to_string(),
            mark,
            tool: None,
            usage: None,
            elapsed_secs,
            session: None,
            detail: None,
        }
    }

    fn column(name: &str, mark: CardMark, cards: Vec<Card>) -> Column {
        Column {
            name: name.to_string(),
            mark,
            state: mark.badge(),
            stage: Some(name.to_string()),
            cards,
        }
    }

    fn rendered(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test backend terminal");
        terminal
            .draw(|frame| draw(frame, app))
            .expect("draw must succeed against a TestBackend");
        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for row in 0..buffer.area.height {
            for col in 0..buffer.area.width {
                out.push_str(buffer[(col, row)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn a_stage_becomes_a_card_and_an_agent_a_sub_card() {
        let _theme = theme::test_guard();
        let app = app_with(workflow(vec![
            stage(
                "collect",
                StageState::Running,
                vec![agent("scanner", AgentState::Running)],
            ),
            stage(
                "synthesis",
                StageState::Pending,
                vec![agent("writer", AgentState::Pending)],
            ),
        ]));

        let content = rendered(&app, 120, 30);

        assert!(content.contains("mission-control"), "got:\n{content}");
        assert!(content.contains("collect"), "got:\n{content}");
        assert!(content.contains("synthesis"), "got:\n{content}");
        assert!(content.contains("scanner"), "got:\n{content}");
        assert!(content.contains("writer"), "got:\n{content}");
        assert!(content.contains("┌"), "les cartes sont bordées:\n{content}");
    }

    /// Le contrat v2 : un état est un symbole **et** un mot anglais, jamais
    /// l'un sans l'autre — une couleur seule ne se lit pas en `mono`, un kanji
    /// seul ne se lit pas du tout.
    #[test]
    fn every_state_reads_as_a_symbol_and_an_english_word() {
        let expected = [
            (CardMark::Pending, "○ pending"),
            (CardMark::Running, "● running"),
            (CardMark::Gate, "◔ gate"),
            (CardMark::Paused, "‖ paused"),
            (CardMark::Done, "✓ done"),
            (CardMark::Failed, "✗ failed"),
            (CardMark::Cancelled, "⊘ cancelled"),
        ];
        for (mark, badge) in expected {
            assert_eq!(mark.badge(), badge);
        }
    }

    /// La porte est l'information du stage, pas de l'agent : un agent en
    /// attente sous une gate ouverte porte `◔ gate`, pas le rond des
    /// non-démarrés.
    #[test]
    fn an_open_gate_marks_the_cards_of_its_stage() {
        assert_eq!(
            agent_mark(&AgentState::Pending, &StageState::Waiting),
            CardMark::Gate
        );
        assert_eq!(
            agent_mark(&AgentState::Pending, &StageState::Paused),
            CardMark::Paused
        );
        assert_eq!(
            agent_mark(&AgentState::Pending, &StageState::Running),
            CardMark::Pending
        );
        assert_eq!(
            agent_mark(&AgentState::Running, &StageState::Waiting),
            CardMark::Running,
            "un agent en vol n'attend aucune porte"
        );
    }

    /// Une pause posée avant le premier point d'arrêt ne vit que dans la table
    /// de l'exécuteur : sans elle, la vue afficherait « pending » d'un stage
    /// que plus rien ne fera partir.
    #[test]
    fn a_pause_requested_before_the_stage_started_still_reads_as_paused() {
        let waiting = stage("deploy", StageState::Pending, vec![]);
        let paused = HashSet::from(["deploy".to_string()]);

        assert_eq!(stage_mark(&waiting, &paused), CardMark::Paused);
        assert_eq!(stage_mark(&waiting, &HashSet::new()), CardMark::Pending);
    }

    #[test]
    fn a_card_without_a_ledger_row_says_so_rather_than_showing_zero() {
        let mut card = card("scanner", CardMark::Running, 75);
        card.usage = None;

        assert_eq!(usage_line(&card), "炭 — · 1m15s");
    }

    #[test]
    fn a_card_with_a_ledger_row_reads_tokens_cost_and_duration() {
        let mut card = card("scanner", CardMark::Done, 12);
        card.usage = Some(AgentUsage {
            input: 12_300,
            output: 450,
            cost: Some(0.42),
        });

        assert_eq!(usage_line(&card), "炭 12k↑ 450↓ · $0.42 · 12s");
    }

    /// Le budget se compte en cellules : un nom japonais coupé sur des chars
    /// déborderait sur la carte voisine, où le `Paragraph` le rognerait sans
    /// le `…` qui signale la coupe.
    #[test]
    fn a_sub_card_never_overflows_its_frame_whatever_the_script() {
        for name in ["監査を実行するエージェント", "👩‍🚀👩‍🚀👩‍🚀👩‍🚀👩‍🚀", "audit"]
        {
            let mut card = card(name, CardMark::Running, 3);
            card.tool = Some("developer__shell_with_a_very_long_name".to_string());
            for width in [8usize, 12, 20, 26] {
                for line in agent_card_lines(&card, width, false) {
                    assert_eq!(
                        line.width(),
                        width,
                        "{name:?} à {width} : {} cellules",
                        line.width()
                    );
                }
            }
        }
    }

    /// Une carte de stage remplit exactement la largeur qu'on lui donne — c'est
    /// ce qui l'empêche de mordre sur la carte voisine — et se ferme sur ses
    /// agents sans jamais dépasser la hauteur qu'on lui laisse.
    #[test]
    fn a_stage_card_fills_exactly_its_width_and_closes_on_its_agents() {
        let column = column(
            "brainstorm",
            CardMark::Running,
            vec![card("ideas", CardMark::Running, 12)],
        );
        for width in [10usize, 20, 30] {
            for rows in [2usize, 5, 8, 14] {
                let lines = stage_card_lines(&column, width, rows, Some(0));
                assert!(lines.len() <= rows, "{width}×{rows} : {}", lines.len());
                assert!(lines.len() >= STAGE_FRAME_ROWS, "{width}×{rows}");
                for line in &lines {
                    assert_eq!(line.width(), width, "{width}×{rows}");
                }
                assert!(
                    lines
                        .last()
                        .is_some_and(|line| line.to_string().ends_with(['┘', '┛'])),
                    "la carte se referme : {width}×{rows}"
                );
            }
        }
        assert_eq!(
            stage_card_lines(&column, 30, 14, Some(0)).len(),
            STAGE_FRAME_ROWS + AGENT_CARD_ROWS,
            "un seul agent : la carte ne s'étire pas sur la hauteur libre"
        );
    }

    /// La sélection se voit sans couleur : le tracé lourd est ce qui distingue
    /// la carte sous le curseur en thème `mono` comme sur un terminal sans
    /// couleur.
    #[test]
    fn the_selected_card_carries_a_heavy_frame() {
        let _theme = theme::test_guard();
        let column = column(
            "brainstorm",
            CardMark::Running,
            vec![card("ideas", CardMark::Running, 12)],
        );

        let selected: String = stage_card_lines(&column, 26, 8, Some(0))
            .iter()
            .map(Line::to_string)
            .collect();
        let idle: String = stage_card_lines(&column, 26, 8, None)
            .iter()
            .map(Line::to_string)
            .collect();

        assert!(selected.contains('┏'), "{selected}");
        assert!(!idle.contains('┏'), "{idle}");
    }

    /// L'anti-confusion n°1 : une porte ouverte se dit en toutes lettres, avec
    /// la touche qui la tranche.
    #[test]
    fn a_waiting_gate_raises_a_full_width_banner_naming_the_key() {
        let board = Board {
            title: "demo".to_string(),
            columns: vec![
                column("brainstorm", CardMark::Done, vec![]),
                column("validation", CardMark::Gate, vec![]),
            ],
        };

        let banner = banner_text(&board, None).expect("une gate attend");

        assert!(
            banner.contains("gate \"validation\" is waiting"),
            "{banner}"
        );
        assert!(banner.contains("press g to decide"), "{banner}");
        assert!(banner.contains(theme::GATE_GLYPH), "{banner}");
    }

    /// Plusieurs portes : la plus ancienne est nommée, les autres comptées —
    /// une bannière qui n'en nommerait aucune n'aiderait à rien.
    #[test]
    fn several_waiting_gates_name_the_oldest_and_count_the_rest() {
        let board = Board {
            title: "demo".to_string(),
            columns: vec![
                column("first", CardMark::Gate, vec![]),
                column("second", CardMark::Gate, vec![]),
                column("third", CardMark::Gate, vec![]),
            ],
        };

        let banner = banner_text(&board, None).expect("trois gates attendent");

        assert!(banner.contains("\"first\""), "{banner}");
        assert!(banner.contains("(+2 more)"), "{banner}");
    }

    /// La réponse à une action prend la bannière le temps de sa péremption : le
    /// plein écran cache le chat, un refus silencieux serait un non-événement.
    #[test]
    fn an_action_notice_takes_over_the_banner() {
        let board = Board {
            title: "demo".to_string(),
            columns: vec![column("validation", CardMark::Gate, vec![])],
        };

        let banner = banner_text(&board, Some("gate « validation » denied")).expect("une notice");

        assert!(banner.contains("denied"), "{banner}");
        assert!(!banner.contains("press g"), "{banner}");
    }

    /// Une notice multi-ligne casserait la bannière d'une seule ligne.
    #[test]
    fn a_multiline_notice_is_flattened_into_one_banner_line() {
        let board = Board {
            title: "demo".to_string(),
            columns: vec![],
        };

        let banner = banner_text(&board, Some("gate « de\nploy » left open")).expect("une notice");

        assert!(!banner.contains('\n'), "{banner:?}");
        assert!(banner.contains('␊'), "{banner:?}");
    }

    /// Le pied ne promet que ce qui marchera : pas de `g` sans porte ouverte,
    /// pas de `p` sur une lame libre.
    #[test]
    fn the_footer_only_offers_the_keys_the_selection_answers() {
        let gated = Board {
            title: "demo".to_string(),
            columns: vec![column(
                "validation",
                CardMark::Gate,
                vec![card("verdict", CardMark::Gate, 0)],
            )],
        };
        let running = Board {
            title: "demo".to_string(),
            columns: vec![column(
                "brainstorm",
                CardMark::Running,
                vec![card("ideas", CardMark::Running, 3)],
            )],
        };
        let free = Board {
            title: FREE_STAGE.to_string(),
            columns: vec![Column {
                name: FREE_STAGE.to_string(),
                mark: CardMark::Running,
                state: CardMark::Running.badge(),
                stage: None,
                cards: vec![card("blade", CardMark::Running, 3)],
            }],
        };

        assert!(footer_keys(&gated, 0, 0, 200).contains("g gate"));
        assert!(!footer_keys(&running, 0, 0, 200).contains("g gate"));
        assert!(footer_keys(&running, 0, 0, 200).contains("p pause"));
        assert!(!footer_keys(&free, 0, 0, 200).contains("p pause"));
    }

    /// Les touches sont **toujours** visibles : sous la largeur du pied complet
    /// il se raccourcit, il ne disparaît pas.
    #[test]
    fn the_footer_degrades_but_never_vanishes() {
        let board = Board {
            title: "demo".to_string(),
            columns: vec![column(
                "brainstorm",
                CardMark::Running,
                vec![card("ideas", CardMark::Running, 3)],
            )],
        };

        for width in [10u16, 30, 40, 80, 200] {
            let footer = footer_keys(&board, 0, 0, width);
            assert!(!footer.trim().is_empty(), "à {width} : {footer:?}");
            assert!(footer.contains("Esc"), "à {width} : {footer:?}");
        }
    }

    /// Le header agrège ce que les cartes disent une à une, et son badge dit ce
    /// qui bloque : une porte ouverte passe devant un stage qui tourne.
    #[test]
    fn the_header_aggregates_the_board_and_a_waiting_gate_wins_its_badge() {
        let mut running = card("ideas", CardMark::Running, 12);
        running.usage = Some(AgentUsage {
            input: 3_400,
            output: 237,
            cost: Some(0.02),
        });
        let board = Board {
            title: "demo".to_string(),
            columns: vec![
                column("brainstorm", CardMark::Running, vec![running]),
                column(
                    "validation",
                    CardMark::Gate,
                    vec![card("verdict", CardMark::Gate, 0)],
                ),
            ],
        };

        let summary = header_summary(&board);

        assert!(summary.contains("◔ gate"), "{summary}");
        assert!(summary.contains("2 stages"), "{summary}");
        assert!(summary.contains("炭 3.4k↑ 237↓"), "{summary}");
        assert!(summary.contains("$0.02"), "{summary}");
        assert!(summary.contains("12s"), "{summary}");
    }

    /// Aucune ligne au ledger : le header dit `炭 —`, pas un zéro qui ferait
    /// passer un run non mesuré pour un run gratuit.
    #[test]
    fn a_board_without_a_ledger_row_aggregates_to_a_dash() {
        let board = Board {
            title: "demo".to_string(),
            columns: vec![column(
                "brainstorm",
                CardMark::Running,
                vec![card("ideas", CardMark::Running, 3)],
            )],
        };

        assert!(
            header_summary(&board).contains("炭 —"),
            "{}",
            header_summary(&board)
        );
    }

    /// Le panneau de détails est ce qui remplit l'espace vide : il porte en
    /// clair ce que la sous-carte tronque.
    #[test]
    fn the_details_panel_spells_out_the_selected_card() {
        let _theme = theme::test_guard();
        let mut selected = card("ideas", CardMark::Done, 61);
        selected.session = Some("20260907_181500".to_string());
        selected.usage = Some(AgentUsage {
            input: 3_400,
            output: 237,
            cost: Some(0.02),
        });
        let board = Board {
            title: "demo".to_string(),
            columns: vec![column("brainstorm", CardMark::Done, vec![selected])],
        };

        let text: String = details_lines(&board, 0, 0, 30)
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("ideas"), "{text}");
        assert!(text.contains("✓ done"), "{text}");
        assert!(text.contains("brainstorm"), "{text}");
        assert!(text.contains("3.4k↑ 237↓"), "{text}");
        assert!(text.contains("$0.02 · 1m01s"), "{text}");
        assert!(text.contains("20260907_181500"), "{text}");
        assert!(text.contains("demo"), "{text}");
    }

    /// Une cause d'échec est rendue telle que l'exécuteur l'a nommée — c'est la
    /// seule surface où elle tient.
    #[test]
    fn the_details_panel_names_the_failure_cause() {
        let mut failed = card("ideas", CardMark::Failed, 3);
        failed.detail = Some("provider timeout".to_string());
        let board = Board {
            title: "demo".to_string(),
            columns: vec![column("brainstorm", CardMark::Failed, vec![failed])],
        };

        let text: String = details_lines(&board, 0, 0, 30)
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("provider timeout"), "{text}");
    }

    #[test]
    fn a_failed_agent_carries_the_cause_the_executor_named() {
        assert_eq!(
            failure_detail(&AgentState::Failed(FailureCause::Error(
                "provider timeout".to_string()
            ))),
            Some("provider timeout".to_string())
        );
        assert_eq!(
            failure_detail(&AgentState::Failed(FailureCause::Budget(
                kaji::workflow::BudgetLimit::Tokens
            ))),
            Some("budget max_tokens exceeded".to_string())
        );
        assert_eq!(failure_detail(&AgentState::Done), None);
    }

    /// Le panneau n'existe qu'à partir de cent colonnes : plus étroit, il
    /// volerait aux cartes la largeur qui les rend lisibles, et ⏎ donne déjà la
    /// même fiche en pleine page.
    #[test]
    fn the_details_panel_only_opens_from_a_hundred_columns() {
        assert_eq!(details_width(80), 0);
        assert_eq!(details_width(99), 0);
        assert_eq!(details_width(100), DETAILS_WIDTH);
        assert_eq!(details_width(200), DETAILS_WIDTH);
    }

    #[test]
    fn the_details_panel_is_rendered_from_a_hundred_columns_and_not_below() {
        let _theme = theme::test_guard();
        let mut app = app_with(workflow(vec![stage(
            "collect",
            StageState::Running,
            vec![agent("scanner", AgentState::Running)],
        )]));
        app.mission.workflow.as_mut().expect("un workflow").stages[0].agents[0].session_id =
            Some("20260907_1815".to_string());

        let wide = rendered(&app, 100, 30);
        assert!(wide.contains(theme::SUBAGENT_GLYPH), "got:\n{wide}");
        assert!(wide.contains("cost/dur"), "got:\n{wide}");
        assert!(wide.contains("20260907_1815"), "got:\n{wide}");

        let narrow = rendered(&app, 80, 30);
        assert!(
            !narrow.contains("cost/dur"),
            "sous cent colonnes, ⏎ garde la fiche lecteur:\n{narrow}"
        );
    }

    #[test]
    fn the_free_column_holds_the_summons_of_a_session_without_a_workflow() {
        let _theme = theme::test_guard();
        let mut app = App::new(None);
        app.mission.open = true;
        app.forge.tasks.insert(
            "t1".to_string(),
            ForgeTask {
                id: "t1".to_string(),
                description: "audit the tests".to_string(),
                status: ForgeStatus::Running,
                current_tool: Some("developer__shell".to_string()),
                elapsed_secs: 9,
                turns: 1,
                result: None,
                error: None,
                seq: 0,
            },
        );

        let content = rendered(&app, 120, 30);

        assert!(content.contains(FREE_STAGE), "got:\n{content}");
        assert!(content.contains("audit the tests"), "got:\n{content}");
        assert!(
            content.contains("developer__shell"),
            "l'outil courant vit au panneau de détails:\n{content}"
        );
    }

    fn blade(id: &str, description: &str) -> ForgeTask {
        ForgeTask {
            id: id.to_string(),
            description: description.to_string(),
            status: ForgeStatus::Running,
            current_tool: None,
            elapsed_secs: 3,
            turns: 1,
            result: None,
            error: None,
            seq: 0,
        }
    }

    /// La colonne libre liste les vraies délégations, sans filtre : un agent de
    /// workflow n'est **pas** une lame summon. Le `SubagentRunner` appelle
    /// `run_subagent_task` en direct, et seul l'outil `delegate` inscrit une
    /// lame dans les `background_tasks` d'où sort le snapshot du volet — les
    /// deux populations sont donc disjointes en production.
    #[test]
    fn the_free_column_lists_the_delegations_a_workflow_never_creates() {
        let mut app = app_with(workflow(vec![stage(
            "collect",
            StageState::Running,
            vec![agent("scanner", AgentState::Running)],
        )]));
        app.forge
            .tasks
            .insert("free-1".to_string(), blade("free-1", "audit by hand"));
        app.forge
            .tasks
            .insert("free-2".to_string(), blade("free-2", "reread the spec"));

        let board = board(&app);

        assert_eq!(board.columns.len(), 2, "un stage plus la colonne libre");
        let stage_column = &board.columns[0];
        assert_eq!(
            stage_column
                .cards
                .iter()
                .map(|card| card.key.as_str())
                .collect::<Vec<_>>(),
            vec!["scanner"],
            "l'agent du DAG vit sur la carte de son stage"
        );
        let free = &board.columns[1];
        assert_eq!(free.name, FREE_STAGE);
        assert_eq!(
            free.cards
                .iter()
                .map(|card| card.key.as_str())
                .collect::<Vec<_>>(),
            vec!["free-1", "free-2"],
            "les lames du volet sont les délégations, toutes rendues"
        );
    }

    /// Sans lame au volet, le plateau n'ajoute pas une colonne vide : une
    /// colonne « unattached » à zéro carte volerait de la largeur aux stages.
    #[test]
    fn the_free_column_only_appears_when_a_blade_escapes_the_workflow() {
        let app = app_with(workflow(vec![stage(
            "collect",
            StageState::Running,
            vec![agent("scanner", AgentState::Running)],
        )]));

        assert_eq!(board(&app).columns.len(), 1);
    }

    /// La cible d'une carte, c'est sa clé brute — un agent adressé par son nom
    /// affiché (assaini, tronqué en cellules) ne serait pas trouvé par
    /// l'exécuteur.
    #[test]
    fn a_card_targets_its_agent_or_its_blade_by_raw_key() {
        let mut app = app_with(workflow(vec![stage(
            "collect",
            StageState::Running,
            vec![agent("scan\tner", AgentState::Running)],
        )]));
        app.forge
            .tasks
            .insert("free-1".to_string(), blade("free-1", "audit"));
        let board = board(&app);

        assert_eq!(
            target(&board, 0, 0),
            Some(MissionTarget::Agent {
                stage: "collect".to_string(),
                agent: "scan\tner".to_string(),
            })
        );
        assert_eq!(
            target(&board, 1, 0),
            Some(MissionTarget::Blade {
                id: "free-1".to_string()
            })
        );
        assert_eq!(target(&board, 9, 0), None, "hors plateau");
    }

    /// La timeline v2 raconte les stages, pas les agents : c'est la maille du
    /// workflow, et un stage qui court porte une lame animée.
    #[test]
    fn the_timeline_scales_the_bars_on_the_longest_stage() {
        let board = Board {
            title: "review".to_string(),
            columns: vec![
                column(
                    "long",
                    CardMark::Running,
                    vec![card("a", CardMark::Running, 100)],
                ),
                column("short", CardMark::Done, vec![card("b", CardMark::Done, 25)]),
            ],
        };

        let lines = timeline_lines(&board, 60, 4);

        let long = lines[0].to_string().matches(BAR_FULL).count();
        let short = lines[1].to_string().matches(BAR_FULL).count();
        assert!(long > 0 && short > 0, "{long} / {short}");
        assert!(
            (short as f64 - (long + 1) as f64 / 4.0).abs() <= 1.5,
            "un quart de la durée, à l'arrondi d'une cellule près : {long} / {short}"
        );
        assert!(
            theme::BLADE_FRAMES.contains(&lines[0].to_string().chars().nth(15).unwrap_or(' '))
                || lines[0]
                    .to_string()
                    .chars()
                    .any(|c| theme::BLADE_FRAMES.contains(&c)),
            "le stage qui court porte une lame animée : {:?}",
            lines[0].to_string()
        );
    }

    /// Un stage qui n'a pas démarré n'a pas de barre : une barre vide se lirait
    /// comme une durée nulle mesurée.
    #[test]
    fn a_pending_stage_gets_a_dash_and_no_bar() {
        let board = Board {
            title: "review".to_string(),
            columns: vec![
                column(
                    "running",
                    CardMark::Running,
                    vec![card("a", CardMark::Running, 40)],
                ),
                column(
                    "later",
                    CardMark::Pending,
                    vec![card("b", CardMark::Pending, 0)],
                ),
            ],
        };

        let lines = timeline_lines(&board, 60, 4);
        let pending = lines[1].to_string();

        assert!(!pending.contains(BAR_FULL), "{pending:?}");
        assert!(!pending.contains(BAR_EMPTY), "{pending:?}");
        assert!(pending.contains('─'), "{pending:?}");
        assert!(pending.contains("0s"), "{pending:?}");
    }

    #[test]
    fn the_timeline_marks_the_stages_it_could_not_draw() {
        let board = Board {
            title: "review".to_string(),
            columns: (0..8)
                .map(|rank| {
                    column(
                        &format!("stage-{rank}"),
                        CardMark::Done,
                        vec![card("a", CardMark::Done, 10)],
                    )
                })
                .collect(),
        };

        let lines = timeline_lines(&board, 60, 4);

        assert_eq!(lines.len(), 4, "le bandeau tient dans la hauteur donnée");
        assert!(lines[3].to_string().contains("+5"), "{:?}", lines[3]);
    }

    /// La hauteur que [`timeline_rows`] réserve est exactement celle que
    /// [`timeline_lines`] consomme dans son cadre : sans quoi le bandeau
    /// rognerait sa dernière barre ou laisserait une ligne vide sous lui.
    #[test]
    fn the_timeline_fills_exactly_the_rows_it_reserved() {
        for stages in 1..=9usize {
            let board = Board {
                title: "review".to_string(),
                columns: (0..stages)
                    .map(|rank| {
                        column(
                            &format!("stage-{rank}"),
                            CardMark::Done,
                            vec![card("a", CardMark::Done, 10)],
                        )
                    })
                    .collect(),
            };
            let rows = usize::from(timeline_rows(&board, 40)).saturating_sub(STAGE_FRAME_ROWS);

            assert_eq!(timeline_lines(&board, 60, rows).len(), rows, "{stages}");
        }
    }

    /// La dégradation en largeur : la vue montre moins de stages, elle ne
    /// déborde jamais — le patron `fits` de la barre d'état.
    #[test]
    fn the_board_never_overflows_from_eighty_to_two_hundred_columns() {
        let _theme = theme::test_guard();
        let mut app = app_with(workflow(
            (0..6)
                .map(|rank| {
                    stage(
                        &format!("stage-{rank}"),
                        StageState::Running,
                        vec![agent(&format!("agent-{rank}"), AgentState::Running)],
                    )
                })
                .collect(),
        ));

        for width in [80u16, 100, 120, 200] {
            app.mission.stage = 0;
            let content = rendered(&app, width, 30);
            for line in content.lines() {
                assert_eq!(
                    line.chars().count(),
                    usize::from(width),
                    "à {width} colonnes : {line:?}"
                );
            }
        }
    }

    /// La bannière tient toute la largeur de la vue — c'est ce qui la rend
    /// impossible à manquer.
    #[test]
    fn the_gate_banner_spans_the_full_width() {
        let _theme = theme::test_guard();
        let app = app_with(workflow(vec![
            stage(
                "brainstorm",
                StageState::Done,
                vec![agent("ideas", AgentState::Done)],
            ),
            stage(
                "validation",
                StageState::Waiting,
                vec![agent("verdict", AgentState::Pending)],
            ),
        ]));

        for width in [80u16, 100, 120, 200] {
            let content = rendered(&app, width, 30);
            let banner = content
                .lines()
                .find(|line| line.contains("is waiting"))
                .unwrap_or_else(|| panic!("pas de bannière à {width}:\n{content}"));
            assert!(banner.contains("press g to decide"), "à {width}: {banner}");
        }
    }

    /// Une vue ouverte sur rien explique où trouver du travail plutôt que
    /// d'afficher un cadre vide.
    #[test]
    fn an_idle_session_explains_where_the_work_comes_from() {
        let _theme = theme::test_guard();
        let mut app = App::new(None);
        app.mission.open = true;

        let content = rendered(&app, 120, 30);

        assert!(content.contains("no workflow running"), "got:\n{content}");
        assert!(content.contains("/workflow"), "got:\n{content}");
        assert!(content.contains("Ctrl+F"), "got:\n{content}");
    }

    #[test]
    fn a_narrow_board_says_how_many_stages_it_hides() {
        let board = workflow_board(
            &workflow(
                (0..6)
                    .map(|rank| {
                        stage(
                            &format!("stage-{rank}"),
                            StageState::Running,
                            vec![agent("a", AgentState::Running)],
                        )
                    })
                    .collect(),
            ),
            &HashMap::new(),
            &HashSet::new(),
        );

        assert!(hidden_marker(&board, 0, 80).ends_with('›'));
        assert!(hidden_marker(&board, 5, 80).contains('‹'));
        assert_eq!(hidden_marker(&board, 0, 400), "");
    }

    #[test]
    fn the_window_slides_to_keep_the_selected_stage_visible() {
        assert_eq!(first_column(0, 2, 6), 0);
        assert_eq!(first_column(2, 2, 6), 1);
        assert_eq!(first_column(5, 2, 6), 4);
        assert_eq!(first_column(3, 6, 6), 0, "tout tient : rien ne glisse");
    }

    /// `stage_card_lines` fait glisser sa fenêtre de sous-cartes avec la même
    /// formule que `first_column` fait glisser ses cartes — testée ici
    /// directement sur `sliding_window_start`, la fonction que les deux
    /// partagent.
    #[test]
    fn the_shared_window_handles_head_middle_and_tail() {
        assert_eq!(sliding_window_start(0, 2, 6), 0, "curseur en tête");
        assert_eq!(sliding_window_start(3, 2, 6), 2, "curseur au milieu");
        assert_eq!(sliding_window_start(5, 2, 6), 4, "curseur en queue");
        assert_eq!(
            sliding_window_start(2, 6, 3),
            0,
            "moins d'éléments que de slots : rien ne glisse"
        );
    }

    /// Les sous-cartes d'un stage glissent comme les cartes d'un plateau : le
    /// curseur reste visible, jamais poussé en tête tant qu'il reste des cartes
    /// en dessous.
    #[test]
    fn stage_card_lines_slides_its_agent_window() {
        let column = column(
            "free",
            CardMark::Running,
            (0..5)
                .map(|rank| card(&format!("agent-{rank}"), CardMark::Running, 3))
                .collect(),
        );
        let rows = STAGE_FRAME_ROWS + 2 * AGENT_CARD_ROWS;
        let names = |lines: &[Line<'static>]| -> Vec<String> {
            (0..5)
                .filter(|rank| {
                    lines
                        .iter()
                        .any(|line| line.to_string().contains(&format!("agent-{rank}")))
                })
                .map(|rank| format!("agent-{rank}"))
                .collect()
        };

        assert_eq!(
            names(&stage_card_lines(&column, 30, rows, Some(0))),
            vec!["agent-0", "agent-1"],
            "curseur en tête"
        );
        assert_eq!(
            names(&stage_card_lines(&column, 30, rows, Some(2))),
            vec!["agent-1", "agent-2"],
            "curseur au milieu"
        );
        assert_eq!(
            names(&stage_card_lines(&column, 30, rows, Some(4))),
            vec!["agent-3", "agent-4"],
            "curseur en queue"
        );
    }

    #[test]
    fn visible_columns_never_reports_zero_when_there_is_a_stage() {
        assert_eq!(visible_columns(200, 6), 6);
        assert_eq!(visible_columns(70, 6), 2);
        assert_eq!(visible_columns(10, 6), 1);
        assert_eq!(visible_columns(200, 0), 0);
    }

    /// Un terminal court garde ses cartes : le bandeau est ce qui cède.
    #[test]
    fn a_short_terminal_drops_the_timeline_before_the_cards() {
        let board = free_board(
            &[&ForgeTask {
                id: "t1".to_string(),
                description: "audit".to_string(),
                status: ForgeStatus::Running,
                current_tool: None,
                elapsed_secs: 3,
                turns: 1,
                result: None,
                error: None,
                seq: 0,
            }],
            &HashMap::new(),
        );

        assert_eq!(timeline_rows(&board, 10), 0);
        assert_eq!(timeline_rows(&board, 20), 3);
    }

    #[test]
    fn a_terminal_too_small_to_hold_anything_does_not_panic() {
        let _theme = theme::test_guard();
        let app = app_with(workflow(vec![stage(
            "collect",
            StageState::Waiting,
            vec![agent("scanner", AgentState::Pending)],
        )]));
        for size in [(1u16, 1u16), (4, 2), (12, 4), (20, 6), (40, 10), (100, 3)] {
            rendered(&app, size.0, size.1);
        }
    }
}
