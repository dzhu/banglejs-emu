use std::{
    io,
    time::{Duration, Instant},
};

use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, EventStream},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use futures_timer::Delay;
use log::info;
use tokio::{
    select,
    sync::{
        broadcast::Receiver,
        mpsc::{UnboundedReceiver, UnboundedSender},
    },
};
use tui::{
    backend::{Backend, CrosstermBackend},
    layout::{Alignment, Rect},
    widgets::{Block, Borders},
    Terminal,
};

use crate::{
    emu::{Input, Output, Screen},
    futures_extras::OptionFuture,
    tui_extras::{Blocked, Console, TuiScreen},
};

#[derive(Debug)]
pub enum UIInput {
    Quit,
    Reset,
    Interrupt,
    EmuInput(Input),
}

pub async fn run_tui(
    mut rx: UnboundedReceiver<Output>,
    tx: UnboundedSender<UIInput>,
    mut quit: Receiver<()>,
) -> anyhow::Result<()> {
    // Set up terminal.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    fn draw<B: Backend>(
        terminal: &mut Terminal<B>,
        screen: &Option<Screen>,
        output: &[u8],
    ) -> io::Result<(u16, u16)> {
        let mut screen_ofs = (0, 0);
        terminal.draw(|f| {
            let w1 = 178;
            let w2 = 80;

            let width = f.size().width;
            let height = f.size().height;

            let (w1, w2) = if width >= w1 + w2 {
                (w1, width - w1)
            } else {
                (width * w1 / (w1 + w2), width * w2 / (w1 + w2))
            };

            if let Some(screen) = screen {
                let screen = Blocked::new(
                    Block::default()
                        .title("Screen")
                        .title_alignment(Alignment::Center)
                        .borders(Borders::ALL),
                    TuiScreen::new(screen),
                );
                f.render_stateful_widget(screen, Rect::new(0, 0, w1, height), &mut screen_ofs);
            }

            let output = Blocked::new(
                Block::default()
                    .title("Console")
                    .title_alignment(Alignment::Center)
                    .borders(Borders::ALL),
                Console::new(String::from_utf8_lossy(output)),
            );
            f.render_widget(output, Rect::new(w1, 0, w2, height));
        })?;
        Ok(screen_ofs)
    }

    let send_string = |data: Vec<u8>| tx.send(UIInput::EmuInput(Input::Console(data))).unwrap();

    let mut screen_ofs = (0, 0);
    let mut output_buf = vec![];
    let mut screen: Option<Screen> = None;
    let mut events = EventStream::new();
    let mut button_deadline = None;
    let mut interrupt_deadline = None;
    let mut reset_deadline = None;

    loop {
        let button_timeout: OptionFuture<_> = button_deadline
            .map(|d| Delay::new(d - Instant::now()))
            .into();
        let reset_timeout: OptionFuture<_> = reset_deadline
            .map(|d| Delay::new(d - Instant::now()))
            .into();
        let interrupt_timeout: OptionFuture<_> = interrupt_deadline
            .map(|d| Delay::new(d - Instant::now()))
            .into();
        select! {
            _ = quit.recv() => break,
            output = rx.recv() => {
                match output {
                    Some(Output::Screen(s)) => {
                        screen = Some(*s);
                        screen_ofs = draw(&mut terminal, &screen, &output_buf)?;
                    }
                    Some(Output::Console(data)) => {
                        output_buf.extend(data);
                        screen_ofs = draw(&mut terminal, &screen, &output_buf)?;
                    }
                    None => break,
                }
            }
            ev = events.next() => {
                match ev.unwrap().unwrap() {
                    Event::Key(k) => {
                        use event::KeyCode::*;
                        match k.code {
                            Left => send_string(b"\x10Bangle.emit('swipe', -1, 0);\n".to_vec()),
                            Right => send_string(b"\x10Bangle.emit('swipe', 1, 0);\n".to_vec()),
                            Up => send_string(b"\x10Bangle.emit('swipe', 0, -1);\n".to_vec()),
                            Down => send_string(b"\x10Bangle.emit('swipe', 0, 1);\n".to_vec()),
                            Enter => {
                                // Since we don't get key-up events in the
                                // terminal, hold the button for a fixed amount
                                // of time after we get a key event; key repeat
                                // will make holding the key down act like
                                // holding the button down.
                                if button_deadline.is_none() {
                                    tx.send(UIInput::EmuInput(Input::Button(true))).unwrap();
                                    let now = Instant::now();
                                    reset_deadline = Some(now + Duration::from_millis(1500));
                                    interrupt_deadline = Some(now + Duration::from_millis(2000));
                                }

                                button_deadline = Some(Instant::now() + Duration::from_millis(300));
                            }
                            Char('q') | Esc => tx.send(UIInput::Quit)?,
                            _ => {}
                        }
                    }
                    Event::Mouse(m) => {
                        use event::MouseEventKind::*;
                        let x = m.column.saturating_sub(screen_ofs.0).clamp(0, 175) as u8;
                        let y = (m.row * 2).saturating_sub(screen_ofs.1).clamp(0, 175) as u8;
                        match m.kind {
                            Down(_) => tx.send(UIInput::EmuInput(Input::Touch(x, y, true)))?,
                            Up(_) => tx.send(UIInput::EmuInput(Input::Touch(x, y, false)))?,
                            Drag(_) => tx.send(UIInput::EmuInput(Input::Touch(x, y, true)))?,
                            Moved => {}
                            ScrollDown => {}
                            ScrollUp => {}
                        }
                    }
                    Event::Resize(..) => {
                        screen_ofs = draw(&mut terminal, &screen, &output_buf)?;
                    }
                    _ => {}
                }
            }
            _ = button_timeout => {
                tx.send(UIInput::EmuInput(Input::Button(false))).unwrap();
                interrupt_deadline = None;
                reset_deadline = None;
                button_deadline = None;
            }
            _ = reset_timeout => {
                info!("reset timeout firing");
                tx.send(UIInput::Reset).unwrap();
                reset_deadline = None;
            }
            _ = interrupt_timeout => {
                info!("interrupt timeout firing");
                tx.send(UIInput::Interrupt).unwrap();
                interrupt_deadline = None;
            }
        }
    }

    // Restore terminal.
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    Ok(())
}
