use std::{
    env,
    fs::{self, File},
    io,
    path::Path,
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

    emu.start(input_rx, output_tx);

    // Set up terminal.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Run UI loop.
    for s in [
        format!(
            "require('Storage').write('.bootcde', atob('{}'));\n",
            read_b64(format!("{BANGLE_APPS}/apps/boot/bootloader.js"))?
        )
        .into_bytes(),
        r#"require('Storage').write('antonclk.info', '{"type":"clock","src":"antonclk.app.js"}')"#
            .to_owned()
            .into_bytes(),
        format!(
            "require('Storage').write('antonclk.app.js', atob('{}'));load()\n",
            read_b64(format!("{BANGLE_APPS}/apps/antonclk/app.js"))?
        )
        .into_bytes(),
    ] {
        for ch in s {
            input_tx.send(ch).unwrap();
        }
    }

    loop {
        let screen: Box<Screen>;
        if let Ok(output) = output_rx.try_recv() {
            if let runner::Output::Screen(s) = output {
                screen = s;

                terminal.draw(|f| {
                    let size = f.size();
                    let block = TuiScreen::new(&screen);
                    f.render_widget(block, size);
                })?;
            }
        } else if let Ok(true) = event::poll(Duration::from_millis(10)) {
            if let Event::Key(_) = event::read().unwrap() {
                break;
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
