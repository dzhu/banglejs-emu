use std::{
    path::Path,
    thread::{self, JoinHandle},
    time::Duration,
};

use crossbeam_channel::{Receiver, Sender};

use crate::emu::{Emulator, Output, BTN1};

pub struct ThreadRunner {
    emu: Emulator,
}

impl ThreadRunner {
    pub fn new<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        Ok(Self {
            emu: Emulator::new(path)?,
        })
    }

    pub fn start(
        self,
        input: Receiver<u8>,
        output: Sender<Output>,
    ) -> JoinHandle<anyhow::Result<()>> {
        thread::spawn(move || {
            let mut emu = self.emu;
            let cb = |ch| output.send(Output::Console(ch)).unwrap();

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
                    output.send(Output::Screen(Box::new(screen))).unwrap();
                }
                emu.handle_io(cb)?;
                let mut timeout = Duration::from_millis(delay.max(10));
                while let Ok(ch) = input.recv_timeout(timeout) {
                    emu.push_string([ch])?;
                    timeout = Duration::from_millis(1);
                }
            }
        })
    }
}
