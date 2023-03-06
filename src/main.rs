use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

use base64::{engine::general_purpose, Engine};
use clap::Parser;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use emu::Screen;
use env_logger::{Builder, Target};
use log::{debug, error, info};
use serde_derive::Deserialize;
use tui::{backend::CrosstermBackend, Terminal};
use tui_screen::TuiScreen;

mod emu;
mod runner;
mod tui_screen;

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

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let config = Config::read(&args.config_path)?;

    Builder::from_default_env()
        .format_timestamp_micros()
        .target(Target::Pipe(Box::new(
            File::create("/tmp/emu.log").expect("Can't create file"),
        )))
        .init();

    // Set up emulator.
    let emu = runner::ThreadRunner::new(&args.wasm_path)?;

    let (input_tx, input_rx) = crossbeam_channel::unbounded();
    let (output_tx, output_rx) = crossbeam_channel::unbounded();

    let (console_tx, console_rx) = crossbeam_channel::unbounded();

    emu.start(input_rx, output_tx);

    // Run network thread.
    thread::spawn({
        let listener = TcpListener::bind(args.bind.as_deref().unwrap_or("127.0.0.1:37026"))?;
        let input_tx = input_tx.clone();
        move || loop {
            listener.set_nonblocking(true).unwrap();
            loop {
                let Ok((mut sock, addr)) = listener.accept() else {
                while console_rx.recv_timeout(Duration::from_millis(50)).is_ok() {}
                continue;
            };
                info!("got connection from {addr}");
                let mut buf = vec![0u8; 4096];
                sock.set_read_timeout(Some(Duration::from_millis(50)))
                    .unwrap();
                loop {
                    let r = sock.read(&mut buf);
                    debug!("sock read: {r:?}");
                    match r {
                        Ok(0) => break,
                        Ok(n) => {
                            for &c in &buf[..n] {
                                input_tx.send(c).unwrap();
                            }
                        }
                        Err(err) => match err.kind() {
                            io::ErrorKind::WouldBlock => {
                                while let Ok(data) =
                                    console_rx.recv_timeout(Duration::from_millis(50))
                                {
                                    sock.write_all(&[data]).unwrap();
                                }
                            }
                            io::ErrorKind::NotConnected => break,
                            x => {
                                error!("unexpected socket err: {x}");
                                break;
                            }
                        },
                    }
                }
            }
        }
    });

    // Set up terminal.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Run UI loop.
    let send_string = move |s: &[u8]| {
        for &ch in s {
            input_tx.send(ch).unwrap();
        }
    };
    fn b64(b: &[u8]) -> String {
        general_purpose::STANDARD_NO_PAD.encode(b)
    }

    for (path, contents) in &config.storage {
        let contents = match contents {
            FileContents::Path(p) => fs::read(p)?,
            FileContents::Contents(s) => s.clone().into_bytes(),
        };
        send_string(
            format!(
                "\x10require('Storage').write(atob('{}'), atob('{}'));\n",
                b64(path.as_bytes()),
                b64(&contents)
            )
            .as_bytes(),
        )
    }

    if let Some(s) = &config.startup {
        send_string(s.as_bytes());
    }

    loop {
        let screen: Box<Screen>;
        if let Ok(output) = output_rx.try_recv() {
            match output {
                runner::Output::Screen(s) => {
                    screen = s;

                    terminal.draw(|f| {
                        let size = f.size();
                        let block = TuiScreen::new(&screen);
                        f.render_widget(block, size);
                    })?;
                }
                runner::Output::Console(c) => console_tx.send(c).unwrap(),
            }
        } else if let Ok(true) = event::poll(Duration::from_millis(10)) {
            if let Event::Key(k) = event::read().unwrap() {
                use event::KeyCode::*;
                match k.code {
                    Left => send_string(b"\x10Bangle.emit('swipe', -1, 0);\n"),
                    Right => send_string(b"\x10Bangle.emit('swipe', 1, 0);\n"),
                    Up => send_string(b"\x10Bangle.emit('swipe', 0, -1);\n"),
                    Down => send_string(b"\x10Bangle.emit('swipe', 0, 1);\n"),
                    Char('q') | Esc => break,
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
