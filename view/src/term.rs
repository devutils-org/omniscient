//! The interactive loop: a terminal, keys, and the live run's updates.
//!
//! The screen is drawn on `/dev/tty` rather than on stdout, so a launched
//! program's output can be sent elsewhere (`omn top -- prog > prog.log`)
//! without taking the screen with it.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::crossterm::{cursor, execute};
use ratatui::Terminal;

use crate::app::App;
use crate::feed::{self, Update};

/// The longest a screen goes without being redrawn while a live run sends
/// updates: often enough to watch, rarely enough to cost little.
const REFRESH: Duration = Duration::from_millis(250);

/// Runs the view until the user quits. `updates` is the live run's, `None`
/// for a replay; `quit` is raised when the user quits, for the runner to
/// stop on. The end of a live run does not end the view: the run's final
/// figures stay on screen until the user quits.
pub fn run(mut app: App, updates: Option<Receiver<Update>>, quit: &AtomicBool) -> io::Result<()> {
    let tty = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
    let mut terminal = enter(tty)?;
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = leave_raw();
        previous(info);
    }));
    // Measured on this thread, which is the one that draws.
    app.cost = crate::app::Cost::Measured(crate::cost::Meter::start());
    let result = event_loop(&mut app, &mut terminal, updates);
    let left = leave(&mut terminal);
    let _ = std::panic::take_hook();
    quit.store(true, Ordering::SeqCst);
    result.and(left)
}

fn enter(mut tty: File) -> io::Result<Terminal<CrosstermBackend<File>>> {
    enable_raw_mode()?;
    execute!(tty, EnterAlternateScreen, cursor::Hide)?;
    Terminal::new(CrosstermBackend::new(tty))
}

fn leave(terminal: &mut Terminal<CrosstermBackend<File>>) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, cursor::Show)?;
    terminal.backend_mut().flush()
}

/// What a panic leaves the terminal in, undone as far as can be without the
/// terminal itself.
fn leave_raw() -> io::Result<()> {
    disable_raw_mode()?;
    let mut tty = OpenOptions::new().write(true).open("/dev/tty")?;
    execute!(tty, LeaveAlternateScreen, cursor::Show)
}

fn event_loop(
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<File>>,
    mut updates: Option<Receiver<Update>>,
) -> io::Result<()> {
    let mut drawn_at: Option<Instant> = None;
    let mut dirty = true;
    loop {
        if let Some(receiver) = &updates {
            loop {
                match receiver.try_recv() {
                    Ok(Update::Abort) => return Ok(()),
                    Ok(update) => {
                        feed::apply(&mut app.data, update);
                        dirty = true;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        updates = None;
                        break;
                    }
                }
            }
        }
        let due = drawn_at.is_none_or(|at| at.elapsed() >= REFRESH);
        // The corner's cost is a measurement too, and a screen nothing
        // changes on is still redrawn for it, once a second.
        if drawn_at.is_some_and(|at| at.elapsed() >= Duration::from_secs(1)) {
            dirty = true;
        }
        if dirty && due {
            terminal.draw(|frame| app.draw(frame))?;
            drawn_at = Some(Instant::now());
            dirty = false;
        }
        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    app.key(key);
                    // A key is answered at once, not at the next refresh.
                    drawn_at = None;
                    dirty = true;
                }
                Event::Resize(..) => {
                    terminal.autoresize()?;
                    drawn_at = None;
                    dirty = true;
                }
                _ => {}
            }
        }
        if app.quit {
            return Ok(());
        }
    }
}
