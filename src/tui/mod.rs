mod app;
mod change_preview;
mod components;
mod syntax;
mod view;

use anyhow::{Context, Result, bail};
use app::App;
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io::{self, IsTerminal, Stdout};
use std::path::Path;
use std::time::Duration;

pub fn run(root: &Path) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("interactive mode requires a terminal; run code-sanity --help to list commands");
    }
    let mut session = TerminalSession::new()?;
    run_loop(&mut session.terminal, root)
}

fn run_loop(terminal: &mut Terminal<CrosstermBackend<Stdout>>, root: &Path) -> Result<()> {
    let mut app = App::new(root);
    while !app.should_quit {
        app.poll_workers();
        terminal.draw(|frame| view::render(frame, &mut app))?;
        if event::poll(Duration::from_millis(80)).context("poll terminal input")? {
            match event::read().context("read terminal input")? {
                Event::Key(key) if key.kind == KeyEventKind::Press => app.handle_key(key),
                Event::Mouse(mouse) => app.handle_mouse(mouse),
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
    }
    Ok(())
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    fn new() -> Result<Self> {
        enable_raw_mode().context("enable terminal raw mode")?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
            let _ = disable_raw_mode();
            return Err(error).context("enter alternate screen");
        }
        let terminal = match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => terminal,
            Err(error) => {
                let _ = disable_raw_mode();
                let mut stdout = io::stdout();
                let _ = execute!(stdout, LeaveAlternateScreen, DisableMouseCapture);
                return Err(error).context("initialize terminal");
            }
        };
        Ok(Self { terminal })
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        );
        let _ = self.terminal.show_cursor();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn bare_tui_state_can_open_on_uninitialized_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let mut app = App::new(temp.path());
        assert!(!app.workspace.initialized);
        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(app.should_quit);
    }
}
