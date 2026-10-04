//! The interactive loop: a terminal, keys, and the live run's updates.
//!
//! The screen is drawn on `/dev/tty` rather than on stdout, so a launched
//! program's output can be sent elsewhere (`omn top -- prog > prog.log`)
//! without taking the screen with it.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind,
};
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

/// The terminal the view draws on, opened and in raw mode.
///
/// Opened before anything is collected, by whoever starts the run, so that a
/// run with no terminal to draw on -- an agent shell, CI, `nohup`, a `setsid`
/// with stdin closed -- is refused at once with the reason, rather than
/// collecting for its whole duration and throwing it away when the drawing
/// thread finds out.
pub struct Screen {
    terminal: Option<Terminal<CrosstermBackend<File>>>,
}

/// Why there is no screen to draw on, in words that say what to do instead.
pub fn no_screen(error: &io::Error) -> String {
    format!(
        "omn top draws on the terminal, and this process has none it can open (/dev/tty:          {error}); `omn top --headless -o DIR ...` collects without one, and `omn top --replay          DIR` draws it afterwards"
    )
}

impl Screen {
    /// Opens `/dev/tty` and takes it over: raw mode, the alternate screen,
    /// the cursor hidden. Whatever fails half way is undone.
    pub fn open() -> io::Result<Screen> {
        let tty = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
        match enter(tty) {
            Ok(terminal) => Ok(Screen {
                terminal: Some(terminal),
            }),
            Err(error) => {
                let _ = leave_raw();
                Err(error)
            }
        }
    }

    /// Runs the view until the user quits. `updates` is the live run's,
    /// `None` for a replay; `quit` is raised when the view ends, for the
    /// runner to stop on -- on every way out, an error and a panic included,
    /// so a run is never left collecting for a view that is gone. `elsewhere`
    /// is the cpu the view causes on other threads (the live run's hand-over
    /// of events), counted in its cost. The end of a live run does not end
    /// the view: the run's final figures stay on screen until the user quits.
    pub fn run(
        mut self,
        mut app: App,
        updates: Option<Receiver<Update>>,
        quit: &AtomicBool,
        elsewhere: Option<Arc<AtomicU64>>,
    ) -> io::Result<()> {
        let _raise = Raise(quit);
        let Some(mut terminal) = self.terminal.take() else {
            return Ok(());
        };
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = leave_raw();
            previous(info);
        }));
        // Measured on this thread, which is the one that draws.
        app.cost = crate::app::Cost::Measured(crate::cost::Meter::start_with(elsewhere));
        let result = event_loop(&mut app, &mut terminal, updates);
        let left = leave(&mut terminal);
        let _ = std::panic::take_hook();
        result.and(left)
    }
}

impl Drop for Screen {
    /// A screen opened and never run -- the run refused before the view
    /// started -- gives the terminal back.
    fn drop(&mut self) {
        if let Some(mut terminal) = self.terminal.take() {
            let _ = leave(&mut terminal);
        }
    }
}

/// Raises `quit` when dropped: on every way out of the view.
struct Raise<'a>(&'a AtomicBool);

impl Drop for Raise<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Opens the terminal and runs the view on it: a replay's whole life.
pub fn run(app: App, updates: Option<Receiver<Update>>, quit: &AtomicBool) -> io::Result<()> {
    let screen = match Screen::open() {
        Ok(screen) => screen,
        Err(error) => {
            quit.store(true, Ordering::SeqCst);
            return Err(error);
        }
    };
    screen.run(app, updates, quit, None)
}

fn enter(mut tty: File) -> io::Result<Terminal<CrosstermBackend<File>>> {
    enable_raw_mode()?;
    execute!(tty, EnterAlternateScreen, cursor::Hide)?;
    Terminal::new(CrosstermBackend::new(tty))
}

fn leave(terminal: &mut Terminal<CrosstermBackend<File>>) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen,
        cursor::Show
    )?;
    terminal.backend_mut().flush()
}

/// What a panic leaves the terminal in, undone as far as can be without the
/// terminal itself.
fn leave_raw() -> io::Result<()> {
    disable_raw_mode()?;
    let mut tty = OpenOptions::new().write(true).open("/dev/tty")?;
    execute!(tty, DisableMouseCapture, LeaveAlternateScreen, cursor::Show)
}

fn event_loop(
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<File>>,
    mut updates: Option<Receiver<Update>>,
) -> io::Result<()> {
    let mut drawn_at: Option<Instant> = None;
    let mut dirty = true;
    // Whether the terminal reports the mouse, kept in step with `app.mouse`
    // (`M` toggles it): crossterm asks for SGR reports (mode 1006), which
    // tmux passes through with `set -g mouse on`.
    let mut captured = false;
    loop {
        if app.mouse != captured {
            if app.mouse {
                execute!(terminal.backend_mut(), EnableMouseCapture)?;
            } else {
                execute!(terminal.backend_mut(), DisableMouseCapture)?;
            }
            captured = app.mouse;
        }
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
                // Capture reports every motion; only a press or the wheel
                // changes anything, and only they cost a redraw.
                Event::Mouse(mouse)
                    if !matches!(
                        mouse.kind,
                        event::MouseEventKind::Moved
                            | event::MouseEventKind::Drag(_)
                            | event::MouseEventKind::Up(_)
                    ) =>
                {
                    app.mouse(mouse);
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
