use std::{
    collections::HashMap,
    fmt::Debug,
    fs::{self, File},
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    str,
};

use anyhow::Context;
use base64::{engine::general_purpose, Engine};
use clap::Parser;
use env_logger::{Builder, Target};
use log::{debug, error, info};
use serde_derive::Deserialize;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, ToSocketAddrs},
    select,
    sync::{
        broadcast::{self, Receiver},
        mpsc::{self, UnboundedReceiver, UnboundedSender},
    },
};

mod emu;
mod option_future;
mod runner;
mod tui_extras;
mod ui;

use crate::{
    emu::{Emulator, Input, Output},
    runner::AsyncRunner,
    ui::UIInput,
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

async fn run_net(
    bind: impl ToSocketAddrs + Debug,
    mut rx: UnboundedReceiver<Vec<u8>>,
    tx: UnboundedSender<Input>,
    mut quit: Receiver<()>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(&bind)
        .await
        .with_context(|| format!("Failed to bind {bind:?}"))?;
    let mut socket: Option<TcpStream> = None;
    let mut buf = vec![0u8; 4096];

    loop {
        let sock_read: option_future::OptionFuture<_> =
            socket.as_mut().map(|s| s.read(&mut buf)).into();
        select! {
            _ = quit.recv() => break,
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
            data = rx.recv() => {
                if let Some(socket) = &mut socket {
                    let _ = socket.write_all(&data.unwrap()).await;
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
                        tx.send(Input::Console(buf[..n].to_owned())).unwrap();
                    }
                    Err(err) => {
                        error!("socket err: {err}");
                        socket = None;
                    }
                }
            }
        }
    }

    Ok(())
}

async fn run_emu(
    emu: Emulator,
    rx: UnboundedReceiver<Input>,
    tx: UnboundedSender<Output>,
    mut quit: Receiver<()>,
) -> anyhow::Result<()> {
    let emu = AsyncRunner::new(emu);
    select! {
        _ = quit.recv() => Ok(()),
        ret = emu.run(rx, tx) => ret,
    }
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

    // Initialize emulator from arguments.
    let args = Args::parse();
    let config = Config::read(&args.config_path)
        .with_context(|| format!("Failed to open config file {:?}", args.config_path))?;

    // Set up independent tasks and channels between them.
    let (to_emu_tx, to_emu_rx) = mpsc::unbounded_channel();
    let (from_emu_tx, mut from_emu_rx) = mpsc::unbounded_channel();
    let (to_ui_tx, to_ui_rx) = mpsc::unbounded_channel();
    let (from_ui_tx, mut from_ui_rx) = mpsc::unbounded_channel();
    let (to_net_tx, to_net_rx) = mpsc::unbounded_channel();
    let (from_net_tx, mut from_net_rx) = mpsc::unbounded_channel();

    let (quit_tx, _) = broadcast::channel(1);

    let mut emu = if let Some(f) = &config.flash_initial_contents_file {
        let flash = get_flash_initial_contents(f)?;
        Emulator::new_with_flash(&args.wasm_path, &flash)?
    } else {
        Emulator::new(&args.wasm_path)?
    };

    if config.factory_reset {
        emu.reset_storage()?;
    }

    let bind = args.bind.as_deref().unwrap_or("127.0.0.1:37026").to_owned();

    let emu_handle = tokio::spawn(run_emu(emu, to_emu_rx, from_emu_tx, quit_tx.subscribe()));
    let net_handle = tokio::spawn(run_net(bind, to_net_rx, from_net_tx, quit_tx.subscribe()));
    let ui_handle = tokio::spawn(ui::run_tui(to_ui_rx, from_ui_tx, quit_tx.subscribe()));

    // Set up initial emulator state as specified by config.
    let send_string = |s: Vec<u8>| {
        to_emu_tx.send(Input::Console(s)).unwrap();
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

    // Run main loop.
    loop {
        select! {
            output = from_emu_rx.recv() => {
                let output = output.unwrap();
                if let Output::Console(data) = &output {
                    info!("output: {:?}", str::from_utf8(data));
                    let _ = to_net_tx.send(data.to_owned());
                }
                let _ = to_ui_tx.send(output);
            }
            data = from_net_rx.recv() => {
                to_emu_tx.send(data.unwrap()).unwrap();
            }
            input = from_ui_rx.recv() => {
                match input.unwrap() {
                    UIInput::Quit => break,
                    UIInput::EmuInput(input) => to_emu_tx.send(input).unwrap(),
                }
            }
        }
    }

    drop(quit_tx);

    emu_handle.await.unwrap().unwrap();
    net_handle.await.unwrap().unwrap();
    ui_handle.await.unwrap().unwrap();

    Ok(())
}
