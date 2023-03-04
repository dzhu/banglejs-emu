use std::{env, fs, path::Path, thread, time::Duration};

use base64::{engine::general_purpose, Engine};
use log::info;

mod emu;

fn main() -> anyhow::Result<()> {
    env_logger::init();

    let mut emu = emu::Emulator::new(env::args().nth(1).unwrap())?;

    info!("==== init");
    emu.init()?;
    emu.send_pin_watch_event(emu::BTN1)?;

    emu.push_string(b"echo(0);\n")?;
    emu.handle_io()?;

    emu.push_string(b"console.log(17);LED1.set()\n")?;

    fn read_b64<P: AsRef<Path>>(path: P) -> anyhow::Result<String> {
        Ok(general_purpose::STANDARD_NO_PAD.encode(fs::read(path)?))
    }

    const BANGLE_APPS: &str = env!("BANGLE_APPS");

    emu.push_string(
        format!(
            "require('Storage').write('.bootcde', atob('{}'));\n",
            read_b64(format!("{BANGLE_APPS}/apps/boot/bootloader.js"))?
        )
        .bytes(),
    )?;
    emu.handle_io()?;

    emu.push_string(
        r#"require('Storage').write('antonclk.info', '{"id":"antonclk","name":"Anton Clock","type":"clock","src":"antonclk.app.js","icon":"antonclk.img","version":"0.11","tags":"clock","files":"antonclk.info,antonclk.app.js"}')"#.bytes(),
    )?;
    emu.handle_io()?;

    emu.push_string(
        format!(
            "require('Storage').write('antonclk.app.js', atob('{}'));\n",
            read_b64(format!("{BANGLE_APPS}/apps/antonclk/app.js"))?
        )
        .bytes(),
    )?;
    emu.handle_io()?;

    for step in 0..2 {
        info!("==== step {step}");
        let ret = emu.idle()?;
        info!("-> {ret:?}");
        emu.handle_io()?;
    }

    emu.draw_screen()?;

    emu.push_string(b"load();\n")?;
    emu.handle_io()?;
    emu.idle()?;

    for i in 0..8 {
        emu.push_string(
            format!(
                "g.setColor({},{},{});g.drawWideLine(3, {}, {}, {}, {});\n",
                i & 1,
                (i >> 1) & 1,
                (i >> 2) & 1,
                20,
                i * 10,
                156,
                i * 10 + 30,
            )
            .bytes(),
        )?;
    }

    emu.draw_screen()?;

    emu.push_string(b"console.log('timeout1'); LED1.set();\n")?;
    emu.push_string(b"console.log(g.drawWideLine, g.vecDraw, g.test, g.test2)")?;
    emu.push_string(b"setTimeout(function() { console.log('timeout2'); LED1.reset(); }, 0);\n")?;
    emu.handle_io()?;

    loop {
        let ret = emu.idle()?;
        info!("idle -> {ret:?}");
        if emu.gfx_changed()? {
            info!("gfx changed");
            emu.draw_screen()?;
        }
        emu.handle_io()?;
        thread::sleep(Duration::from_millis(20));
    }
}
