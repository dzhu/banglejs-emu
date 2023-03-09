use std::time::Duration;

use futures_timer::Delay;
use tokio::{
    select,
    sync::mpsc::{UnboundedReceiver, UnboundedSender},
};

use crate::emu::{Emulator, Input, Output, BTN1};

pub struct AsyncRunner {
    emu: Emulator,
}

impl AsyncRunner {
    pub fn new(emu: Emulator) -> Self {
        Self { emu }
    }

    pub async fn run(
        self,
        mut input: UnboundedReceiver<Input>,
        output: UnboundedSender<Output>,
    ) -> anyhow::Result<()> {
        let mut emu = self.emu;
        let send_output = |chars: Vec<u8>| {
            if !chars.is_empty() {
                let _ = output.send(Output::Console(chars));
            }
        };

        emu.init()?;
        emu.send_pin_watch_event(BTN1)?;
        send_output(emu.handle_io()?);

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
            send_output(emu.handle_io()?);

            let mut first = true;
            loop {
                let timeout =
                    Delay::new(Duration::from_millis(if first { delay.max(10) } else { 1 }));
                first = false;
                select! {
                    _ = timeout => {
                        break;
                    }
                    s = input.recv() => {
                        if let Some(s) = s {
                            match s {
                                Input::Console(s) => emu.push_string(&s)?,
                                Input::Touch(x, y, on) => emu.send_touch(x, y, on)?,
                            }
                        }
                    }
                }
            }
        }
    }
}
