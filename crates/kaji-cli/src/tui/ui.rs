use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

pub fn draw_placeholder(frame: &mut Frame) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(frame.area());
    frame.render_widget(
        Paragraph::new("kaji tui — q pour quitter")
            .block(Block::default().borders(Borders::ALL).title(" chat ")),
        cols[0],
    );
    frame.render_widget(
        Block::default().borders(Borders::ALL).title(" SPEC "),
        cols[1],
    );
}
