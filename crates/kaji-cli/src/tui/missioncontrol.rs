//! 炉 mission-control — la forge en plein écran.
//!
//! Le volet forge (32 colonnes) dit ce que les lames font ; cette vue dit
//! comment elles s'ordonnent. Une colonne par stage du workflow actif, ou une
//! seule colonne « libre » quand la session ne pilote aucun workflow et que les
//! lames viennent de summons isolés.
//!
//! Deux sources, deux autorités, comme dans le volet : le snapshot du workflow
//! (`WorkflowHandle::snapshot`) fait autorité sur les états et la topologie,
//! l'usage ledger sur les tokens et le coût. Rien ici n'invente une mesure — un
//! agent sans ligne au ledger affiche `炭 —`, il n'affiche pas zéro.
//!
//! T7 branche les actions sur cette sélection : Enter ouvre la fiche, `x`
//! annule, `p` suspend un stage, `g` tranche sa gate. Chaque carte porte donc
//! sa **clé** — le nom d'agent ou l'identifiant de lame bruts — à côté du nom
//! affiché, qui est assaini et ne peut pas servir d'adresse.

use crate::tui::app::App;
use crate::tui::forge::{ForgeStatus, ForgeTask};
use crate::tui::ui::{forge_duration, sanitize_for_display};
use crate::tui::{gitstatus, statusbar, theme};
use kaji::workflow::{AgentState, StageState, WorkflowState};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use std::collections::{HashMap, HashSet};

/// La colonne des lames qui ne relèvent d'aucun stage — un summon lancé à la
/// main pendant qu'un workflow tourne ou non.
pub const FREE_STAGE: &str = "libre";

/// Largeur visée d'une colonne de stage : la troisième ligne d'une carte
/// (`炭 12.3k↑ 4.5k↓ · $12.34 · 12m34s`) est ce qui la calibre.
const STAGE_WIDTH: usize = 34;

/// Gouttière entre deux colonnes. Deux cellules : un kanji d'en-tête ne doit
/// jamais toucher la colonne d'à côté.
const STAGE_GAP: usize = 2;

/// Trois lignes par carte plus la respiration qui la sépare de la suivante.
const CARD_ROWS: usize = 4;

/// Les deux lignes d'en-tête d'une colonne : son nom, puis son état.
const COLUMN_HEADER_ROWS: usize = 2;

/// Le champ de marque fait deux cellules, comme un kanji : `門` en occupe deux
/// à lui seul, `✓` une seule et se fait compléter — sans quoi une colonne de
/// portes et de verdicts ne s'aligne plus.
const MARK_CELLS: usize = 2;

/// Ce que `{marque} 遣 ` prend avant le nom d'un agent.
const CARD_INDENT: usize = MARK_CELLS + 1 + 2 + 1;

/// Hauteur minimale sous laquelle le bandeau timeline cède la place aux
/// colonnes : une carte vaut mieux qu'une barre.
const TIMELINE_MIN_HEIGHT: u16 = 14;

/// Barres au bandeau, en plus de son en-tête. Au-delà, une ligne de reste.
const TIMELINE_MAX_BARS: usize = 5;

/// Cellules du libellé d'une barre de timeline.
const TIMELINE_LABEL_CELLS: usize = 20;

/// Cellules réservées au chrono en queue de barre — `12m34s` et sa respiration.
const TIMELINE_DURATION_CELLS: usize = 8;

/// Les touches vraiment branchées, et elles seules : `s` (steer) attend un
/// changement du chemin de spawn partagé avec summon, l'annoncer ici
/// promettrait une action qui ne se produit pas.
const FOOTER: &str =
    " h/l stages · j/k cartes · ⏎ fiche · x annuler · p pause · g gate · q retour ";

const BAR_FULL: char = '█';
const BAR_EMPTY: char = '░';

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
    /// précise : naviguer ou fermer la vue l'efface, et le pied retrouve sa
    /// légende de touches.
    pub notice: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardMark {
    Running,
    Done,
    Failed,
    Cancelled,
    Paused,
    Gate,
    Pending,
}

impl CardMark {
    /// Deux cellules toujours : les glyphes d'une cellule sont complétés à
    /// droite pour que les noms d'agents restent sur la même colonne.
    fn glyph(self, elapsed_secs: u64) -> String {
        let glyph = match self {
            CardMark::Running => {
                theme::blade_frame(std::time::Duration::from_secs(elapsed_secs)).to_string()
            }
            CardMark::Done => "✓".to_string(),
            CardMark::Failed | CardMark::Cancelled => "✗".to_string(),
            CardMark::Paused => "⏸".to_string(),
            CardMark::Gate => theme::GATE_GLYPH.to_string(),
            CardMark::Pending => "◦".to_string(),
        };
        let pad = MARK_CELLS.saturating_sub(gitstatus::display_width(&glyph));
        format!("{glyph}{}", " ".repeat(pad))
    }

    fn style(self) -> Style {
        match self {
            CardMark::Running | CardMark::Failed | CardMark::Gate => theme::accent(),
            CardMark::Done | CardMark::Cancelled | CardMark::Paused | CardMark::Pending => {
                theme::dim()
            }
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
    pub status: String,
    pub tool: Option<String>,
    pub usage: Option<AgentUsage>,
    pub elapsed_secs: u64,
}

#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub state: String,
    /// Le nom brut du stage, `None` pour la colonne des lames libres — c'est
    /// ce qui distingue une carte d'agent de workflow d'un summon isolé.
    pub stage: Option<String>,
    pub cards: Vec<Card>,
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
}

/// Ce que la carte sous le curseur désigne. Les touches d'action de T7 ne
/// consomment que ça : un agent d'un stage se pilote par le
/// `WorkflowHandle`, une lame libre par le summon, et les deux chemins ne se
/// confondent jamais.
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
/// du volet forge. Sans workflow il ne reste que la colonne « libre », rendue
/// même vide — une vue ouverte sur rien doit dire qu'elle n'a rien.
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
/// afficherait « en attente » d'un stage que plus rien ne fera partir.
fn stage_label(stage: &kaji::workflow::StageStatus, paused: &HashSet<String>) -> String {
    if stage.state != StageState::Paused
        && !stage.state.is_terminal()
        && paused.contains(&stage.name)
    {
        return "pause demandée".to_string();
    }
    stage.state.label().to_string()
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
            .map(|stage| Column {
                name: sanitize_for_display(&stage.name),
                state: stage_label(stage, paused),
                stage: Some(stage.name.clone()),
                cards: stage
                    .agents
                    .iter()
                    .map(|agent| Card {
                        key: agent.name.clone(),
                        name: sanitize_for_display(&agent.name),
                        mark: agent_mark(&agent.state, &stage.state),
                        status: agent_status(&agent.state, &stage.state).to_string(),
                        tool: None,
                        usage: agent
                            .session_id
                            .as_deref()
                            .and_then(|id| usage.get(id).copied()),
                        elapsed_secs: (agent.duration_ms.max(0) / 1000) as u64,
                    })
                    .collect(),
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
    Column {
        name: FREE_STAGE.to_string(),
        state: format!("{} lames", tasks.len()),
        stage: None,
        cards: tasks
            .iter()
            .map(|task| Card {
                key: task.id.clone(),
                name: sanitize_for_display(&task.description.replace('\n', "␊")),
                mark: forge_mark(task.status),
                status: forge_label(task.status).to_string(),
                tool: task.current_tool.clone(),
                usage: usage.get(&task.id).copied(),
                elapsed_secs: task.elapsed_secs,
            })
            .collect(),
    }
}

/// L'état du stage l'emporte sur celui de l'agent tant que l'agent n'a pas
/// commencé : une porte ouverte ou une pause décrit ce qui bloque, là où
/// « en attente » ne dirait pas pourquoi.
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

/// Le libellé suit la marque : une carte qui porte 門 doit dire « gate », pas
/// « en attente » — c'est la même information que le mark, et l'agent n'attend
/// pas son tour, il attend une décision.
fn agent_status(agent: &AgentState, stage: &StageState) -> &'static str {
    match (agent, stage) {
        (AgentState::Pending, StageState::Waiting) => StageState::Waiting.label(),
        (AgentState::Pending, StageState::Paused) => StageState::Paused.label(),
        _ => agent.label(),
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

fn forge_label(status: ForgeStatus) -> &'static str {
    match status {
        ForgeStatus::Running => "en cours",
        ForgeStatus::Done => "terminé",
        ForgeStatus::Failed => "échec",
        ForgeStatus::Cancelled => "annulé",
    }
}

/// Combien de colonnes tiennent dans `width` cellules. Au moins une : sous la
/// largeur d'une colonne, elle se rétrécit plutôt que la vue ne disparaisse.
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
/// les colonnes de stages et les cartes d'une colonne — même contrat que le
/// volet forge et l'explorateur.
fn sliding_window_start(selected: usize, visible: usize, total: usize) -> usize {
    if visible == 0 || total <= visible {
        return 0;
    }
    let last = total - visible;
    selected.saturating_sub(visible.saturating_sub(1)).min(last)
}

/// La première colonne rendue.
pub fn first_column(selected: usize, visible: usize, columns: usize) -> usize {
    sliding_window_start(selected, visible, columns)
}

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    frame.render_widget(Clear, area);

    let board = board(app);
    let hidden = hidden_marker(&board, app.mission.stage, area.width);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::border_active())
        .title(Span::styled(
            format!(
                " {} mission-control · {}{hidden} ",
                theme::FORGE_GLYPH,
                board.title
            ),
            theme::title(),
        ))
        .title_bottom(match app.mission.notice.as_deref() {
            // Une notice porte des noms venus de la spec : `sanitize_for_display`
            // laisse passer `\n`, qui casserait ce pied d'une seule ligne.
            Some(notice) => Line::from(format!(
                " {} ",
                sanitize_for_display(&notice.replace('\n', "␊"))
            ))
            .style(theme::accent()),
            None => Line::from(FOOTER).style(theme::dim()),
        });

    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let bars = timeline_rows(&board, inner.height);
    let columns_height = inner.height.saturating_sub(bars);
    draw_columns(
        frame,
        app,
        &board,
        Rect {
            height: columns_height,
            ..inner
        },
    );
    if bars > 0 {
        frame.render_widget(
            Paragraph::new(Text::from(timeline_lines(
                &board,
                usize::from(inner.width),
                usize::from(bars),
            ))),
            Rect {
                y: inner.y + columns_height,
                height: bars,
                ..inner
            },
        );
    }
}

fn draw_columns(frame: &mut Frame, app: &App, board: &Board, area: Rect) {
    if area.height == 0 || board.columns.is_empty() {
        return;
    }
    let width = usize::from(area.width);
    let visible = visible_columns(width, board.columns.len());
    let first = first_column(app.mission.stage, visible, board.columns.len());
    // Une largeur entière par colonne, gouttière comprise : un reste partagé
    // en pourcentage ferait déborder la dernière d'une cellule sur un écran
    // impair, et le `Paragraph` la rognerait sans le dire. Plafonnée à la
    // largeur calibrée : sur un écran très large, étirer trois colonnes à
    // soixante cellules éparpillerait des cartes qui en tiennent trente-quatre.
    let slot = (width / visible).min(STAGE_WIDTH + STAGE_GAP);

    for (slot_index, column_index) in (first..first + visible).enumerate() {
        let Some(column) = board.columns.get(column_index) else {
            break;
        };
        let x = area.x + (slot_index * slot) as u16;
        let column_width = slot.saturating_sub(STAGE_GAP).max(1);
        let selected = (column_index == app.mission.stage).then_some(app.mission.card);
        frame.render_widget(
            Paragraph::new(Text::from(column_lines(
                column,
                column_width,
                usize::from(area.height),
                selected,
            ))),
            Rect {
                x,
                y: area.y,
                width: column_width as u16,
                height: area.height,
            },
        );
    }
}

/// L'en-tête de la colonne, puis autant de cartes que la hauteur en porte —
/// la fenêtre glissant sur la sélection comme celle des colonnes glisse sur le
/// stage.
pub fn column_lines(
    column: &Column,
    width: usize,
    rows: usize,
    selected: Option<usize>,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled(
            gitstatus::truncate_cells(&column.name, width),
            theme::title(),
        )),
        Line::from(Span::styled(
            gitstatus::truncate_cells(&column.state, width),
            theme::dim(),
        )),
    ];

    let slots = rows.saturating_sub(COLUMN_HEADER_ROWS) / CARD_ROWS;
    if slots == 0 {
        return lines;
    }
    if column.cards.is_empty() {
        lines.push(Line::from(Span::styled("— vide —", theme::dim())));
        return lines;
    }

    let cursor = selected.unwrap_or(0);
    let first = sliding_window_start(cursor, slots, column.cards.len());
    for (rank, card) in column.cards.iter().enumerate().skip(first).take(slots) {
        let highlight =
            (Some(rank) == selected).then(|| theme::accent().bg(theme::user_bg_color()));
        lines.extend(card_lines(card, width, highlight));
        lines.push(Line::from(String::new()));
    }
    lines
}

/// Les trois lignes d'une carte : qui, ce qu'elle brûle, ce qu'elle coûte.
pub fn card_lines(card: &Card, width: usize, highlight: Option<Style>) -> Vec<Line<'static>> {
    let name_budget = width.saturating_sub(CARD_INDENT);
    let head = format!(
        "{} {} {}",
        card.mark.glyph(card.elapsed_secs),
        theme::SUBAGENT_GLYPH,
        gitstatus::truncate_cells(&card.name, name_budget)
    );
    // 思 dit qu'une lame réfléchit : une carte en attente ou terminée ne
    // réfléchit pas, elle porte son état nu.
    let tool = match (card.tool.as_deref(), card.mark) {
        (Some(tool), _) => format!("{} {tool}", theme::FIRE_GLYPH),
        (None, CardMark::Running) => format!("{} {}", theme::THINKING_GLYPH, card.status),
        (None, _) => card.status.clone(),
    };
    let indent = " ".repeat(MARK_CELLS + 1);
    let body_budget = width.saturating_sub(indent.len());

    vec![
        Line::from(Span::styled(
            gitstatus::truncate_cells(&head, width),
            highlight.unwrap_or_else(|| card.mark.style()),
        )),
        Line::from(Span::styled(
            format!("{indent}{}", gitstatus::truncate_cells(&tool, body_budget)),
            highlight.unwrap_or_else(theme::dim),
        )),
        Line::from(Span::styled(
            format!(
                "{indent}{}",
                gitstatus::truncate_cells(&usage_line(card), body_budget)
            ),
            highlight.unwrap_or_else(theme::dim),
        )),
    ]
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

/// Hauteur du bandeau : son en-tête plus une barre par agent, plafonnée. Zéro
/// sur un terminal trop court — les cartes passent avant.
fn timeline_rows(board: &Board, height: u16) -> u16 {
    if height < TIMELINE_MIN_HEIGHT {
        return 0;
    }
    let agents = board.cards().count();
    if agents == 0 {
        return 0;
    }
    let bars = agents.min(TIMELINE_MAX_BARS);
    let overflow = usize::from(agents > TIMELINE_MAX_BARS);
    (1 + bars + overflow) as u16
}

/// Le bandeau de pied : une barre par agent, proportionnelle à la durée de la
/// plus longue. Les barres partagent la palette du thème actif — l'accent pour
/// ce qui brûle, le texte pour ce qui a rendu son verdict.
pub fn timeline_lines(board: &Board, width: usize, rows: usize) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        format!("{} timeline", theme::ELAPSED_GLYPH),
        theme::title(),
    ))];
    if rows <= 1 {
        return lines;
    }

    let cards: Vec<&Card> = board.cards().collect();
    let longest = cards
        .iter()
        .map(|card| card.elapsed_secs)
        .max()
        .unwrap_or(0);
    let bar_cells = width
        .saturating_sub(TIMELINE_LABEL_CELLS + 1 + TIMELINE_DURATION_CELLS + 1)
        .max(1);
    // La ligne de reste se prélève sur les barres : un bandeau qui la
    // rajouterait par-dessus déborderait de la hauteur qu'on lui a donnée, et
    // le `Paragraph` mangerait la dernière barre sans le dire.
    let room = rows - 1;
    let shown = if cards.len() > room {
        room.saturating_sub(1)
    } else {
        cards.len()
    };

    for card in cards.iter().take(shown) {
        let filled = if longest == 0 {
            0
        } else {
            // Arrondi au plus proche : une barre plancher rend deux durées
            // voisines identiques là où la moitié d'une cellule les sépare.
            ((card.elapsed_secs as u128 * bar_cells as u128 * 2 + longest as u128)
                / (longest as u128 * 2)) as usize
        };
        let filled = filled.min(bar_cells);
        let label = gitstatus::truncate_cells(&card.name, TIMELINE_LABEL_CELLS);
        let pad = TIMELINE_LABEL_CELLS.saturating_sub(gitstatus::display_width(&label));
        let style = if card.mark == CardMark::Running {
            theme::accent()
        } else {
            theme::text()
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{label}{} ", " ".repeat(pad)), theme::dim()),
            Span::styled(BAR_FULL.to_string().repeat(filled), style),
            Span::styled(
                BAR_EMPTY.to_string().repeat(bar_cells - filled),
                theme::dim(),
            ),
            Span::styled(
                format!(" {}", forge_duration(card.elapsed_secs)),
                theme::dim(),
            ),
        ]));
    }
    if cards.len() > shown {
        lines.push(Line::from(Span::styled(
            format!("… +{}", cards.len() - shown),
            theme::dim(),
        )));
    }
    lines
}

/// Ce que le titre ajoute quand des stages sont hors champ : sans lui, une
/// vue étroite ferait croire que le workflow n'a que les colonnes visibles.
fn hidden_marker(board: &Board, selected: usize, width: u16) -> String {
    let inner = usize::from(width.saturating_sub(2));
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
            workflow: "revue".to_string(),
            stages,
        }
    }

    fn app_with(workflow: WorkflowState) -> App {
        let mut app = App::new(None);
        app.mission.workflow = Some(workflow);
        app.mission.open = true;
        app
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
    fn a_stage_becomes_a_column_and_an_agent_a_card() {
        let _theme = theme::test_guard();
        let app = app_with(workflow(vec![
            stage(
                "collecte",
                StageState::Running,
                vec![agent("scanner", AgentState::Running)],
            ),
            stage(
                "synthese",
                StageState::Pending,
                vec![agent("redacteur", AgentState::Pending)],
            ),
        ]));

        let content = rendered(&app, 120, 30);

        assert!(content.contains("mission-control"), "got:\n{content}");
        assert!(content.contains("collecte"), "got:\n{content}");
        assert!(content.contains("synthese"), "got:\n{content}");
        assert!(content.contains("scanner"), "got:\n{content}");
        assert!(content.contains("redacteur"), "got:\n{content}");
    }

    /// La porte est l'information du stage, pas de l'agent : un agent en
    /// attente sous une gate ouverte porte 門, pas le rond des non-démarrés.
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

    /// Le libellé suit la marque : une carte qui porte 門 dit « gate », sans
    /// quoi elle répéterait « en attente » sur les deux lignes sans jamais
    /// dire ce qui bloque.
    #[test]
    fn the_label_says_what_the_mark_shows() {
        assert_eq!(
            agent_status(&AgentState::Pending, &StageState::Waiting),
            "gate"
        );
        assert_eq!(
            agent_status(&AgentState::Pending, &StageState::Paused),
            "en pause"
        );
        assert_eq!(
            agent_status(&AgentState::Pending, &StageState::Running),
            "en attente"
        );
        assert_eq!(
            agent_status(&AgentState::Running, &StageState::Waiting),
            "en cours"
        );
    }

    /// `門` vaut deux cellules, `✓` une seule : sans complément la colonne des
    /// noms se décalerait d'une carte à l'autre.
    #[test]
    fn every_mark_occupies_two_cells() {
        for mark in [
            CardMark::Running,
            CardMark::Done,
            CardMark::Failed,
            CardMark::Cancelled,
            CardMark::Paused,
            CardMark::Gate,
            CardMark::Pending,
        ] {
            assert_eq!(
                gitstatus::display_width(&mark.glyph(3)),
                MARK_CELLS,
                "{mark:?}"
            );
        }
    }

    #[test]
    fn a_card_without_a_ledger_row_says_so_rather_than_showing_zero() {
        let card = Card {
            key: "k".to_string(),
            name: "scanner".to_string(),
            mark: CardMark::Running,
            status: "en cours".to_string(),
            tool: None,
            usage: None,
            elapsed_secs: 75,
        };

        assert_eq!(usage_line(&card), "炭 — · 1m15s");
    }

    #[test]
    fn a_card_with_a_ledger_row_reads_tokens_cost_and_duration() {
        let card = Card {
            key: "k".to_string(),
            name: "scanner".to_string(),
            mark: CardMark::Done,
            status: "terminé".to_string(),
            tool: None,
            usage: Some(AgentUsage {
                input: 12_300,
                output: 450,
                cost: Some(0.42),
            }),
            elapsed_secs: 12,
        };

        assert_eq!(usage_line(&card), "炭 12k↑ 450↓ · $0.42 · 12s");
    }

    /// Le budget se compte en cellules : un nom japonais coupé sur des chars
    /// déborderait sur la colonne voisine, où le `Paragraph` le rognerait sans
    /// le `…` qui signale la coupe.
    #[test]
    fn a_card_never_overflows_its_column_whatever_the_script() {
        for name in ["監査を実行するエージェント", "👩‍🚀👩‍🚀👩‍🚀👩‍🚀👩‍🚀", "audit"]
        {
            let card = Card {
                key: "k".to_string(),
                name: name.to_string(),
                mark: CardMark::Running,
                status: "en cours".to_string(),
                tool: Some("developer__shell_with_a_very_long_name".to_string()),
                usage: None,
                elapsed_secs: 3,
            };
            for width in [12usize, 20, 34] {
                for line in card_lines(&card, width, None) {
                    assert!(
                        line.width() <= width,
                        "{name:?} à {width} : {} cellules",
                        line.width()
                    );
                }
            }
        }
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
                description: "auditer les tests".to_string(),
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
        assert!(content.contains("auditer les tests"), "got:\n{content}");
        assert!(content.contains("developer__shell"), "got:\n{content}");
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
    /// deux populations sont donc disjointes en production. Le filtre qui
    /// dédoublonnait les deux reposait sur l'invariant inverse : il n'a jamais
    /// rien filtré.
    #[test]
    fn the_free_column_lists_the_delegations_a_workflow_never_creates() {
        let mut app = app_with(workflow(vec![stage(
            "collecte",
            StageState::Running,
            vec![agent("scanner", AgentState::Running)],
        )]));
        app.forge
            .tasks
            .insert("libre-1".to_string(), blade("libre-1", "auditer à la main"));
        app.forge
            .tasks
            .insert("libre-2".to_string(), blade("libre-2", "relire la spec"));

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
            "l'agent du DAG vit sur la colonne de son stage"
        );
        let free = &board.columns[1];
        assert_eq!(free.name, FREE_STAGE);
        assert_eq!(
            free.cards
                .iter()
                .map(|card| card.key.as_str())
                .collect::<Vec<_>>(),
            vec!["libre-1", "libre-2"],
            "les lames du volet sont les délégations, toutes rendues"
        );
    }

    /// Sans lame au volet, le plateau n'ajoute pas une colonne vide : une
    /// colonne « libre » à zéro carte volerait de la largeur aux stages.
    #[test]
    fn the_free_column_only_appears_when_a_blade_escapes_the_workflow() {
        let app = app_with(workflow(vec![stage(
            "collecte",
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
            "collecte",
            StageState::Running,
            vec![agent("scan\tner", AgentState::Running)],
        )]));
        app.forge
            .tasks
            .insert("libre-1".to_string(), blade("libre-1", "auditer"));
        let board = board(&app);

        assert_eq!(
            target(&board, 0, 0),
            Some(MissionTarget::Agent {
                stage: "collecte".to_string(),
                agent: "scan\tner".to_string(),
            })
        );
        assert_eq!(
            target(&board, 1, 0),
            Some(MissionTarget::Blade {
                id: "libre-1".to_string()
            })
        );
        assert_eq!(target(&board, 9, 0), None, "hors plateau");
    }

    #[test]
    fn the_timeline_scales_the_bars_on_the_longest_agent() {
        let board = Board {
            title: "revue".to_string(),
            columns: vec![Column {
                stage: None,
                name: "collecte".to_string(),
                state: "en cours".to_string(),
                cards: vec![
                    Card {
                        key: "k".to_string(),
                        name: "long".to_string(),
                        mark: CardMark::Running,
                        status: "en cours".to_string(),
                        tool: None,
                        usage: None,
                        elapsed_secs: 100,
                    },
                    Card {
                        key: "k".to_string(),
                        name: "court".to_string(),
                        mark: CardMark::Done,
                        status: "terminé".to_string(),
                        tool: None,
                        usage: None,
                        elapsed_secs: 25,
                    },
                ],
            }],
        };

        let lines = timeline_lines(&board, 60, 4);

        let long = lines[1].to_string().matches(BAR_FULL).count();
        let short = lines[2].to_string().matches(BAR_FULL).count();
        assert!(long > 0 && short > 0, "{long} / {short}");
        assert!(
            (short as f64 - long as f64 / 4.0).abs() <= 1.0,
            "un quart de la durée, à l'arrondi d'une cellule près : {long} / {short}"
        );
    }

    #[test]
    fn the_timeline_marks_the_agents_it_could_not_draw() {
        let cards: Vec<Card> = (0..8)
            .map(|rank| Card {
                key: "k".to_string(),
                name: format!("agent-{rank}"),
                mark: CardMark::Done,
                status: "terminé".to_string(),
                tool: None,
                usage: None,
                elapsed_secs: 10,
            })
            .collect();
        let board = Board {
            title: "revue".to_string(),
            columns: vec![Column {
                stage: None,
                name: "collecte".to_string(),
                state: "terminé".to_string(),
                cards,
            }],
        };

        let lines = timeline_lines(&board, 60, 4);

        assert_eq!(lines.len(), 4, "le bandeau tient dans la hauteur donnée");
        assert!(lines[3].to_string().contains("+6"), "{:?}", lines[3]);
    }

    /// La hauteur que [`timeline_rows`] réserve est exactement celle que
    /// [`timeline_lines`] consomme : sans quoi le bandeau rognerait sa dernière
    /// barre ou laisserait une ligne vide sous lui.
    #[test]
    fn the_timeline_fills_exactly_the_rows_it_reserved() {
        for agents in 1..=9usize {
            let board = Board {
                title: "revue".to_string(),
                columns: vec![Column {
                    stage: None,
                    name: "collecte".to_string(),
                    state: "en cours".to_string(),
                    cards: (0..agents)
                        .map(|rank| Card {
                            key: "k".to_string(),
                            name: format!("agent-{rank}"),
                            mark: CardMark::Done,
                            status: "terminé".to_string(),
                            tool: None,
                            usage: None,
                            elapsed_secs: 10,
                        })
                        .collect(),
                }],
            };
            let rows = usize::from(timeline_rows(&board, 40));

            assert_eq!(timeline_lines(&board, 60, rows).len(), rows, "{agents}");
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

        assert_eq!(hidden_marker(&board, 0, 80), " · 4›");
        assert_eq!(hidden_marker(&board, 5, 80), " · ‹4");
        assert_eq!(hidden_marker(&board, 0, 400), "");
    }

    #[test]
    fn the_window_slides_to_keep_the_selected_stage_visible() {
        assert_eq!(first_column(0, 2, 6), 0);
        assert_eq!(first_column(2, 2, 6), 1);
        assert_eq!(first_column(5, 2, 6), 4);
        assert_eq!(first_column(3, 6, 6), 0, "tout tient : rien ne glisse");
    }

    /// `column_lines` fait glisser sa fenêtre de cartes avec la même formule
    /// que `first_column` fait glisser ses colonnes — testée ici directement
    /// sur `sliding_window_start`, la fonction que les deux partagent.
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

    /// Les cartes d'une colonne glissent comme les colonnes d'un plateau : le
    /// curseur reste visible, jamais poussé en tête tant qu'il reste des
    /// cartes en dessous.
    #[test]
    fn column_lines_slides_its_card_window_like_the_stage_columns() {
        let cards: Vec<Card> = (0..5)
            .map(|rank| Card {
                key: "k".to_string(),
                name: format!("agent-{rank}"),
                mark: CardMark::Running,
                status: "en cours".to_string(),
                tool: None,
                usage: None,
                elapsed_secs: 3,
            })
            .collect();
        let column = Column {
            stage: None,
            name: "libre".to_string(),
            state: "en cours".to_string(),
            cards,
        };
        let rows = COLUMN_HEADER_ROWS + 2 * CARD_ROWS;
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
            names(&column_lines(&column, 40, rows, Some(0))),
            vec!["agent-0", "agent-1"],
            "curseur en tête"
        );
        assert_eq!(
            names(&column_lines(&column, 40, rows, Some(2))),
            vec!["agent-1", "agent-2"],
            "curseur au milieu"
        );
        assert_eq!(
            names(&column_lines(&column, 40, rows, Some(4))),
            vec!["agent-3", "agent-4"],
            "curseur en queue"
        );
    }

    #[test]
    fn visible_columns_never_reports_zero_when_there_is_a_stage() {
        assert_eq!(visible_columns(200, 6), 5);
        assert_eq!(visible_columns(78, 6), 2);
        assert_eq!(visible_columns(10, 6), 1);
        assert_eq!(visible_columns(200, 0), 0);
    }

    /// Un terminal court garde ses cartes : le bandeau est ce qui cède.
    #[test]
    fn a_short_terminal_drops_the_timeline_before_the_cards() {
        let board = free_board(
            &[&ForgeTask {
                id: "t1".to_string(),
                description: "auditer".to_string(),
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
        assert_eq!(timeline_rows(&board, 20), 2);
    }

    #[test]
    fn a_terminal_too_small_to_hold_anything_does_not_panic() {
        let _theme = theme::test_guard();
        let app = app_with(workflow(vec![stage(
            "collecte",
            StageState::Running,
            vec![agent("scanner", AgentState::Running)],
        )]));
        for size in [(4u16, 2u16), (12, 4), (20, 6), (40, 10)] {
            rendered(&app, size.0, size.1);
        }
    }
}
