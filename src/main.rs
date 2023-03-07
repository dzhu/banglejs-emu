use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
};

use base64::{engine::general_purpose, Engine};
use clap::Parser;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, EventStream},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use env_logger::{Builder, Target};
use futures::{future::FutureExt, StreamExt};
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
    terminal::CompletedFrame,
    Terminal,
};
use tui_screen::TuiScreen;

mod emu;
mod option_future;
mod runner;
mod tui_screen;

use emu::{Output, Screen};

use crate::runner::AsyncRunner;

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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let config = Config::read(&args.config_path)?;

    Builder::from_default_env()
        .format_timestamp_micros()
        .target(Target::Pipe(Box::new(
            File::create("/tmp/emu.log").expect("Can't create file"),
        )))
        .init();

    // Set up emulator.
    let emu = AsyncRunner::new(&args.wasm_path)?;

    let (input_tx, input_rx) = mpsc::unbounded_channel();
    let (output_tx, mut output_rx) = mpsc::unbounded_channel();

    tokio::spawn(emu.run(input_rx, output_tx));

    // Set up terminal.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Run UI loop.
    let send_string = |s: Vec<u8>| {
        input_tx.send(s).unwrap();
    };
    fn b64(b: &[u8]) -> String {
        general_purpose::STANDARD_NO_PAD.encode(b)
    }

    for (path, contents) in &config.storage {
        let contents = match contents {
            FileContents::Path(p) => fs::read(p)?,
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

    fn draw<'a, B: Backend>(
        terminal: &'a mut Terminal<B>,
        screen: &Screen,
    ) -> io::Result<CompletedFrame<'a>> {
        terminal.draw(|f| {
            let size = f.size();
            let block = TuiScreen::new(screen);
            f.render_widget(block, size);
        })
    }

    let mut events = EventStream::new();
    let mut screen: Option<Screen> = None;
    let mut socket: Option<TcpStream> = None;

    let listener = TcpListener::bind(args.bind.as_deref().unwrap_or("127.0.0.1:37026"))
        .await
        .unwrap();
    let mut buf = vec![0u8; 4096];

    loop {
        let sock_read: option_future::OptionFuture<_> =
            socket.as_mut().map(|s| s.read(&mut buf)).into();
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
                        input_tx.send(buf[..n].to_owned()).unwrap();
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
                        draw(&mut terminal, &s)?;
                        screen = Some(*s);
                    }
                    Output::Console(data) => {
                        if let Some(socket) = &mut socket {
                            let _ = socket.write_all(&data).await;
                        }
                    }
                }
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
                            Char('q') | Esc => break,
                            _ => {}
                        }
                    }
                    Event::Resize(..) => {
                        if let Some(screen) = &screen {
                            draw(&mut terminal, screen)?;
                        }
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
