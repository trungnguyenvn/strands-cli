//! Terminal wrapper — manages raw mode, alt-screen, and the event reader task.
//!
//! Based on the pattern from amazon-q-developer-cli's chat-cli-ui.

use std::io::{self, Stderr};
use std::ops::{Deref, DerefMut};

use crossterm::cursor;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as CrosstermEvent, KeyEventKind,
};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use futures::{FutureExt, StreamExt};
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::event::Event;

pub struct Tui {
    pub terminal: ratatui::Terminal<CrosstermBackend<Stderr>>,
    pub task: JoinHandle<()>,
    pub cancellation_token: CancellationToken,
    pub event_rx: Option<UnboundedReceiver<Event>>,
    pub event_tx: UnboundedSender<Event>,
    pub frame_rate: f64,
    pub tick_rate: f64,
    /// Whether running in fullscreen (alt-screen) mode.
    pub fullscreen: bool,
}

impl Tui {
    pub fn new(tick_rate: f64, frame_rate: f64) -> io::Result<Self> {
        let terminal = ratatui::Terminal::new(CrosstermBackend::new(io::stderr()))?;
        let (event_tx, event_rx) = unbounded_channel();
        let cancellation_token = CancellationToken::new();
        let task = tokio::spawn(async {});

        Ok(Self {
            terminal,
            task,
            cancellation_token,
            event_rx: Some(event_rx),
            event_tx,
            frame_rate,
            tick_rate,
            fullscreen: true,
        })
    }

    fn start(&mut self) {
        let tick_delay = std::time::Duration::from_secs_f64(1.0 / self.tick_rate);
        let render_delay = std::time::Duration::from_secs_f64(1.0 / self.frame_rate);
        self.cancel();
        self.cancellation_token = CancellationToken::new();
        let cancel = self.cancellation_token.clone();
        let tx = self.event_tx.clone();

        self.task = tokio::spawn(async move {
            let mut reader = crossterm::event::EventStream::new();
            let mut tick_interval = tokio::time::interval(tick_delay);
            let mut render_interval = tokio::time::interval(render_delay);

            loop {
                let tick = tick_interval.tick();
                let render = render_interval.tick();
                let crossterm_event = reader.next().fuse();

                tokio::select! {
                    _ = cancel.cancelled() => break,
                    maybe_event = crossterm_event => {
                        if let Some(Ok(evt)) = maybe_event {
                            let event = match evt {
                                CrosstermEvent::Key(key) => {
                                    if key.kind == KeyEventKind::Press {
                                        Some(Event::Key(key))
                                    } else {
                                        None
                                    }
                                }
                                CrosstermEvent::Mouse(mouse) => Some(Event::Mouse(mouse)),
                                CrosstermEvent::Resize(w, h) => Some(Event::Resize(w, h)),
                                CrosstermEvent::Paste(s) => Some(Event::Paste(s)),
                                CrosstermEvent::FocusGained => Some(Event::FocusGained),
                                CrosstermEvent::FocusLost => Some(Event::FocusLost),
                            };
                            if let Some(e) = event {
                                let _ = tx.send(e);
                            }
                        }
                    }
                    _ = tick => { let _ = tx.send(Event::Tick); }
                    _ = render => { let _ = tx.send(Event::Render); }
                }
            }
        });
    }

    /// Enter the TUI. If `fullscreen` is true, uses alternate screen.
    /// If false, stays in the normal terminal (no alt-screen) — mirrors Claude Code's
    /// non-fullscreen mode.
    pub fn enter_with_fullscreen(&mut self, fullscreen: bool) -> io::Result<()> {
        self.fullscreen = fullscreen;
        crossterm::terminal::enable_raw_mode()?;
        if fullscreen {
            crossterm::execute!(
                io::stderr(),
                EnterAlternateScreen,
                EnableMouseCapture,
                EnableBracketedPaste,
                cursor::Hide
            )?;
        } else {
            // Non-fullscreen: no alt-screen, but still enable mouse + paste + hide cursor
            crossterm::execute!(
                io::stderr(),
                EnableMouseCapture,
                EnableBracketedPaste,
                cursor::Hide
            )?;
        }
        self.start();
        Ok(())
    }

    pub fn exit(&mut self) -> io::Result<()> {
        self.cancel();
        if crossterm::terminal::is_raw_mode_enabled()? {
            self.flush()?;
            if self.fullscreen {
                crossterm::execute!(
                    io::stderr(),
                    DisableBracketedPaste,
                    DisableMouseCapture,
                    LeaveAlternateScreen,
                    cursor::Show
                )?;
            } else {
                crossterm::execute!(
                    io::stderr(),
                    DisableBracketedPaste,
                    DisableMouseCapture,
                    cursor::Show
                )?;
            }
            crossterm::terminal::disable_raw_mode()?;
        }
        Ok(())
    }

    pub fn cancel(&self) {
        self.cancellation_token.cancel();
    }
}

impl Deref for Tui {
    type Target = ratatui::Terminal<CrosstermBackend<Stderr>>;
    fn deref(&self) -> &Self::Target {
        &self.terminal
    }
}

impl DerefMut for Tui {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.terminal
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.exit();
    }
}
