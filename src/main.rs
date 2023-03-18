use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, BufRead, BufReader, Read},
    path::{Path, PathBuf},
    str,
    time::{Duration, Instant},
};

use anyhow::Context;
use base64::{engine::general_purpose, Engine};
use clap::Parser;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, EventStream},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use env_logger::{Builder, Target};
use futures::{future::FutureExt, StreamExt};
use futures_timer::Delay;
use log::{debug, error, info};
use serde_derive::Deserialize;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    select,
    sync::mpsc,
};
use tui::{
    backend::{Backend, CrosstermBackend},
    layout::{Alignment, Rect},
    widgets::{Block, Borders, Paragraph},
    Terminal,
};

mod emu;
mod option_future;
mod runner;
mod tui_extras;

use crate::{
    emu::{Emulator, Input, Output, Screen},
    runner::AsyncRunner,
    tui_extras::{Blocked, TuiScreen},
};

#[derive(Clone, Debug, Deserialize)]
enum FileContents {
    #[serde(rename = "path")]
    Path(PathBuf),
    #[serde(rename = "contents")]
    Contents(String),
}

#[derive(Clone, Debug, Deserialize)]
struct Config {
    #[serde(default)]
    factory_reset: bool,
    flash_initial_contents_file: Option<String>,
    #[serde(default)]
    storage: HashMap<String, FileContents>,
    startup: Option<String>,
}

impl Config {
    fn read<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let mut f = File::open(path)?;
        let mut buf = String::new();
        f.read_to_string(&mut buf)?;
        let config: Config = toml::from_str(&buf)?;
        Ok(config)
    }
}

#[derive(Debug, Parser)]
struct Args {
    #[arg(short = 'b')]
    bind: Option<String>,

    config_path: PathBuf,

    wasm_path: PathBuf,
}

fn get_flash_initial_contents<P: AsRef<Path>>(path: P) -> anyhow::Result<Vec<u8>> {
    let f = File::open(path)?;
    let f = BufReader::new(f);

    let mut ret = vec![];

    for line in f.lines() {
        let line = line?;
        let fields = line.split(',');
        let row: Result<Vec<u8>, _> = fields
            .filter(|f| !f.is_empty())
            .map(|f| f.parse())
            .collect();
        if let Ok(row) = row {
            ret.extend(row);
        }
    }

    Ok(ret)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let log_file = "/tmp/emu.log";
    Builder::from_default_env()
        .format_timestamp_micros()
        .target(Target::Pipe(Box::new(
            File::create(log_file)
                .with_context(|| format!("Failed to create log file {log_file}"))?,
        )))
        .init();

    // Start up emulator and network listener.
    let args = Args::parse();
    let config = Config::read(&args.config_path)
        .with_context(|| format!("Failed to open config file {:?}", args.config_path))?;

    let (input_tx, input_rx) = mpsc::unbounded_channel();
    let (output_tx, mut output_rx) = mpsc::unbounded_channel();

    let mut emu = if let Some(f) = &config.flash_initial_contents_file {
        let flash = get_flash_initial_contents(f)?;
        Emulator::new_with_flash(&args.wasm_path, &flash)?
    } else {
        Emulator::new(&args.wasm_path)?
    };

    if config.factory_reset {
        emu.reset_storage()?;
    }

    let emu = AsyncRunner::new(emu);

    tokio::spawn(emu.run(input_rx, output_tx));

    let bind = args.bind.as_deref().unwrap_or("127.0.0.1:37026");
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("Failed to bind {bind}"))?;

    // Set up initial emulator state as specified by config.
    let send_string = |s: Vec<u8>| {
        input_tx.send(Input::Console(s)).unwrap();
    };
    fn b64(b: &[u8]) -> String {
        general_purpose::STANDARD_NO_PAD.encode(b)
    }

    for (path, contents) in &config.storage {
        let contents = match contents {
            FileContents::Path(p) => {
                fs::read(p).with_context(|| format!("Failed to load file {p:?}"))?
            }
            FileContents::Contents(s) => s.clone().into_bytes(),
        };
        info!("writing {} bytes to {}", contents.len(), path);
        send_string(
            format!(
                "\x10require('Storage').write(atob('{}'), atob('{}'));\n",
                b64(path.as_bytes()),
                b64(&contents)
            )
            .into_bytes(),
        )
    }

    if let Some(s) = &config.startup {
        send_string(s.clone().into_bytes());
    }

    // Set up terminal.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Run main loop.
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
                Paragraph::new(String::from_utf8_lossy(output)),
            );
            f.render_widget(output, Rect::new(w1, 0, w2, height));
        })?;
        Ok(screen_ofs)
    }

    let mut events = EventStream::new();
    let mut screen: Option<Screen> = None;
    let mut socket: Option<TcpStream> = None;

    let mut buf = vec![0u8; 4096];
    let mut screen_ofs = (0, 0);
    let mut button_deadline = None;
    let mut output_buf = vec![];

    loop {
        let sock_read: option_future::OptionFuture<_> =
            socket.as_mut().map(|s| s.read(&mut buf)).into();
        let button_timeout: option_future::OptionFuture<_> = button_deadline
            .map(|d| Delay::new(d - Instant::now()))
            .into();
        select! {
            new_conn = listener.accept() => {
                let (s, addr) = new_conn?;
                match socket {
                    Some(_) => {
                        debug!("ignoring connection from {addr}");
                    }
                    None => {
                        info!("got connection from {addr}");
                        socket = Some(s);
                    }
                }
            }
            r = sock_read => {
                debug!("sock read: {r:?}");
                match r {
                    Ok(0) => {
                        debug!("socket connection closed");
                        socket = None;
                    }
                    Ok(n) => {
                        input_tx.send(Input::Console(buf[..n].to_owned())).unwrap();
                    }
                    Err(err) => {
                        error!("socket err: {err}");
                        socket = None;
                    }
                }
            }
            output = output_rx.recv() => {
                match output.unwrap() {
                    Output::Screen(s) => {
                        screen = Some(*s);
                        screen_ofs = draw(&mut terminal, &screen, &output_buf)?;
                    }
                    Output::Console(data) => {
                        info!("output: {:?}", str::from_utf8(&data));
                        if let Some(socket) = &mut socket {
                            let _ = socket.write_all(&data).await;
                        }
                        output_buf.extend(data);
                        screen_ofs = draw(&mut terminal, &screen, &output_buf)?;
                    }
                }
            }
            _ = button_timeout => {
                input_tx.send(Input::Button(false)).unwrap();
                button_deadline = None;
            }
            ev = events.next().fuse() => {
                match ev.unwrap().unwrap() {
                    Event::Key(k) => {
                        use event::KeyCode::*;
                        debug!("key: {k:?}");
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
                                    input_tx.send(Input::Button(true)).unwrap();
                                }
                                button_deadline = Some(Instant::now() + Duration::from_millis(300));
                            }
                            Char('q') | Esc => break,
                            _ => {}
                        }
                    }
                    Event::Mouse(m) => {
                        use event::MouseEventKind::*;
                        let x = m.column.saturating_sub(screen_ofs.0).clamp(0, 175) as u8;
                        let y = (m.row * 2).saturating_sub(screen_ofs.1).clamp(0, 175) as u8;
                        match m.kind {
                            Down(_) => input_tx.send(Input::Touch(x, y, true))?,
                            Up(_) => input_tx.send(Input::Touch(x, y, false))?,
                            Drag(_) => input_tx.send(Input::Touch(x, y, true))?,
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
