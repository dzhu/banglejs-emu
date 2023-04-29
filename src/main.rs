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
mod futures_extras;
mod runner;
mod tui_extras;
mod ui;

use crate::{
    emu::{Emulator, Input, Output},
    futures_extras::{OptionFuture, Task},
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
struct FileSpec {
    #[serde(default)]
    evaluate: bool,

    #[serde(flatten)]
    contents: FileContents,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct Config {
    #[serde(default)]
    factory_reset: bool,
    flash_initial_contents_file: Option<String>,
    #[serde(default)]
    storage: HashMap<String, FileSpec>,
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

    fn build<P: AsRef<Path>>(&self, wasm_path: P) -> anyhow::Result<Emulator> {
        let mut emu = if let Some(f) = &self.flash_initial_contents_file {
            let flash = get_flash_initial_contents(f)?;
            Emulator::new_with_flash(&wasm_path, &flash)?
        } else {
            Emulator::new(&wasm_path)?
        };

        if self.factory_reset {
            emu.reset_storage()?;
        }

        emu.init()?;

        // Set up initial emulator state as specified by config.
        let mut send_string = |s: Vec<u8>| {
            emu.push_string(s.iter()).unwrap();
        };
        fn b64(b: &[u8]) -> String {
            general_purpose::STANDARD_NO_PAD.encode(b)
        }

        for (path, spec) in &self.storage {
            let contents = match &spec.contents {
                FileContents::Path(p) => {
                    fs::read(p).with_context(|| format!("Failed to load file {p:?}"))?
                }
                FileContents::Contents(s) => s.clone().into_bytes(),
            };
            info!("writing {} bytes to {}", contents.len(), path);
            let s = if spec.evaluate {
                format!(
                    "\x10require('Storage').write(atob('{}'), eval(atob('{}')));\n",
                    b64(path.as_bytes()),
                    b64(&contents),
                )
            } else {
                const CHUNK_SIZE: usize = 1 << 15;
                contents
                    .chunks(CHUNK_SIZE)
                    .enumerate()
                    .map(|(ind, chunk)| {
                        format!(
                            "\x10require('Storage').write(atob('{}'), atob('{}'), {}, {});\n",
                            b64(path.as_bytes()),
                            b64(chunk),
                            ind * CHUNK_SIZE,
                            contents.len(),
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("")
            };
            send_string(s.into_bytes())
        }

        if let Some(s) = &self.startup {
            send_string(s.clone().into_bytes());
        }

        Ok(emu)
    }
}

#[derive(Debug, Parser)]
struct Args {
    // These comments should not end in periods due to how they are presented in
    // the CLI help output.
    /// The TCP port to bind to
    #[arg(short = 'b', default_value_t = String::from("localhost:37026"))]
    bind: String,

    /// A config file to use for setting up the emulator
    #[arg(short = 'c')]
    config_path: Option<PathBuf>,

    /// A file to send emulator logging output to
    #[arg(short = 'o')]
    log_file: Option<PathBuf>,

    /// The compiled firmware
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
        let sock_read: OptionFuture<_> = socket.as_mut().map(|s| s.read(&mut buf)).into();
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

async fn _main() -> anyhow::Result<()> {
    let args = Args::parse();

    if let Some(log_file) = args.log_file {
        Builder::from_default_env()
            .format_timestamp_micros()
            .target(Target::Pipe(Box::new(
                File::options()
                    .create(true)
                    .append(true)
                    .open(&log_file)
                    .with_context(|| format!("Failed to create log file {log_file:?}"))?,
            )))
            .init();
    }

    // Initialize emulator from arguments.
    let emu = match &args.config_path {
        Some(path) => Config::read(path)
            .with_context(|| format!("Failed to open config file {:?}", args.config_path))?,
        None => Config::default(),
    }
    .build(&args.wasm_path)?;

    // Set up independent tasks and channels between them.
    let (to_emu_tx, to_emu_rx) = mpsc::unbounded_channel();
    let (from_emu_tx, mut from_emu_rx) = mpsc::unbounded_channel();
    let (to_ui_tx, to_ui_rx) = mpsc::unbounded_channel();
    let (from_ui_tx, mut from_ui_rx) = mpsc::unbounded_channel();
    let (to_net_tx, to_net_rx) = mpsc::unbounded_channel();
    let (from_net_tx, mut from_net_rx) = mpsc::unbounded_channel();

    let (quit_tx, _) = broadcast::channel(1);

    let q = || quit_tx.subscribe();
    let mut emu = Task::spawn(run_emu(emu, to_emu_rx, from_emu_tx, q()));
    let mut net = Task::spawn(run_net(args.bind, to_net_rx, from_net_tx, q()));
    let mut ui = Task::spawn(ui::run_tui(to_ui_rx, from_ui_tx, q()));

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
                if let Some(data) = data {
                    let _ = to_emu_tx.send(data);
                }
            }
            input = from_ui_rx.recv() => {
                match input.unwrap() {
                    UIInput::Quit => break,
                    UIInput::EmuInput(input) => to_emu_tx.send(input).unwrap(),
                }
            }

            _ = &mut emu => break,
            _ = &mut net => break,
            _ = &mut ui => break,
        }
    }

    drop(quit_tx);

    async fn wait<T, E: Debug>(label: &str, task: Task<Result<T, E>>) {
        info!("waiting for {label}...");
        match task.output().await {
            Ok(Ok(_)) => info!("{label} finished!"),
            Ok(Err(e)) => {
                eprintln!("{label} failed: {e:?}");
                error!("{label} failed: {e:?}");
            }
            Err(e) => {
                eprintln!("{label} panicked: {e:?}");
                error!("{label} panicked: {e:?}");
            }
        }
    }

    wait("ui", ui).await;
    wait("emu", emu).await;
    wait("net", net).await;

    info!("done, exiting!");
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let ret = rt.block_on(_main());
    rt.shutdown_background();
    ret
}
