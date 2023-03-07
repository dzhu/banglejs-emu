use std::{path::Path, time::Duration};

use futures_timer::Delay;
use tokio::{
    select,
    sync::mpsc::{UnboundedReceiver, UnboundedSender},
};

use crate::emu::{Emulator, Output, BTN1};

pub struct AsyncRunner {
    emu: Emulator,
}

impl AsyncRunner {
    pub fn new<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        Ok(Self {
            emu: Emulator::new(path)?,
        })
    }

    pub async fn run(
        self,
        mut input: UnboundedReceiver<u8>,
        output: UnboundedSender<Output>,
    ) -> anyhow::Result<()> {
        let mut emu = self.emu;
        let cb = |ch| {
            let _ = output.send(Output::Console(ch));
        };

        emu.init()?;
        emu.send_pin_watch_event(BTN1)?;
        emu.handle_io(cb)?;

        loop {
            let mut delay = 1;
            for _ in 0..5 {
                let d = emu.idle()?;
                if d > 0 {
                    delay = d as u64;
                    break;
                }
            }
            if emu.gfx_changed()? {
                let screen = emu.get_screen()?;
                let _ = output.send(Output::Screen(Box::new(screen)));
            }
            emu.handle_io(cb)?;

            let mut first = true;
            loop {
                let timeout =
                    Delay::new(Duration::from_millis(if first { delay.max(10) } else { 1 }));
                first = false;
                select! {
                    _ = timeout => {
                        break;
                    }
                    ch = input.recv() => {
                        if let Some(ch) = ch {
                            emu.push_string([ch])?;
                        }
                    }
                }
            }
        }
    }
}
