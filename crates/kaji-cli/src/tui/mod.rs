pub mod ui;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use std::path::PathBuf;
use std::time::Duration;

pub async fn run(spec: Option<PathBuf>) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, spec);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, _spec: Option<PathBuf>) -> Result<()> {
    loop {
        terminal.draw(ui::draw_placeholder)?;
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press && key.code == KeyCode::Char('q') {
                    return Ok(());
                }
            }
        }
    }
}
