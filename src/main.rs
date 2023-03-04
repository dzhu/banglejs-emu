use std::{
    env,
    fs::{self, File},
    io::{self, Read, Write},
    net::TcpListener,
    path::Path,
    thread,
    time::Duration,
};

use base64::{engine::general_purpose, Engine};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use emu::Screen;
use env_logger::{Builder, Target};
use log::{debug, error, info};
use tui::{backend::CrosstermBackend, Terminal};
use tui_screen::TuiScreen;

mod emu;
mod runner;
mod tui_screen;

fn main() -> anyhow::Result<()> {
    fn read_b64<P: AsRef<Path>>(path: P) -> anyhow::Result<String> {
        Ok(general_purpose::STANDARD_NO_PAD.encode(fs::read(path)?))
    }

    const BANGLE_APPS: &str = env!("BANGLE_APPS");

    Builder::from_default_env()
        .format_timestamp_micros()
        .target(Target::Pipe(Box::new(
            File::create("/tmp/emu.log").expect("Can't create file"),
        )))
        .init();

    // Set up emulator.
    let emu = runner::ThreadRunner::new(env::args().nth(1).unwrap())?;

    let (input_tx, input_rx) = crossbeam_channel::unbounded();
    let (output_tx, output_rx) = crossbeam_channel::unbounded();

    let (console_tx, console_rx) = crossbeam_channel::unbounded();

    emu.start(input_rx, output_tx);

    // Run network thread.
    let listener = TcpListener::bind("127.0.0.1:37026")?;
    thread::spawn({
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

    for s in [
        format!(
            "require('Storage').write('.bootcde', atob('{}'));\n",
            read_b64(format!("{BANGLE_APPS}/apps/boot/bootloader.js"))?
        ),
        r#"require('Storage').write('antonclk.info', '{"type":"clock","src":"antonclk.app.js"}')"#
            .to_owned(),
        format!(
            "require('Storage').write('antonclk.app.js', atob('{}'));load()\n",
            read_b64(format!("{BANGLE_APPS}/apps/antonclk/app.js"))?
        ),
    ] {
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
