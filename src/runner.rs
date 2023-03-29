use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use futures_timer::Delay;
use log::info;
use tokio::{
    select,
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
};

use crate::{
    emu::{Emulator, Flags, Input, Output, BTN1},
    futures_extras::OptionFuture,
};

pub struct AsyncRunner {
    emu: Emulator,
}

async fn watchdog(
    mut button_rx: UnboundedReceiver<bool>,
    flags: Flags,
    wake_tx: UnboundedSender<()>,
) {
    fn deadline_future(d: Option<Instant>) -> OptionFuture<Delay> {
        d.map(|d| Delay::new(d - Instant::now())).into()
    }

    let mut interrupt_deadline = None;
    let mut reset_deadline = None;
    loop {
        select! {
            button = button_rx.recv() => {
                if button.unwrap() {
                    let now = Instant::now();
                    reset_deadline = Some(now + Duration::from_millis(1500));
                    interrupt_deadline = Some(now + Duration::from_millis(2000));
                } else {
                    interrupt_deadline = None;
                    reset_deadline = None;
                }
            }
            _ = deadline_future(reset_deadline) => {
                info!("reset timeout firing");
                flags.reset.set();
                wake_tx.send(()).unwrap();
                reset_deadline = None;
            }
            _ = deadline_future(interrupt_deadline) => {
                if flags.reset.get() {
                    info!("interrupt timeout firing");
                    flags.interrupt.set();
                } else {
                    info!("reset succeeded, skipping interrupt");
                }
                interrupt_deadline = None;
            }
        }
    }
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
        let (input2_tx, mut input2_rx) = mpsc::unbounded_channel();
        let (to_watchdog_tx, to_watchdog_rx) = mpsc::unbounded_channel();
        let (wake_tx, mut wake_rx) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            while let Some(x) = input.recv().await {
                if let Input::Button(b) = x {
                    to_watchdog_tx.send(b).unwrap();
                }
                input2_tx.send(x).unwrap();
            }
        });
        tokio::spawn(watchdog(to_watchdog_rx, self.emu.flags(), wake_tx));

        let emu = Arc::new(Mutex::new(self.emu));
        let send_output = |chars: Vec<u8>| {
            if !chars.is_empty() {
                let _ = output.send(Output::Console(chars));
            }
        };

        {
            let mut emu = emu.lock().unwrap();
            emu.send_pin_watch_event(BTN1)?;
            send_output(emu.handle_io()?);
        }

        loop {
            let mut delay = 1;
            for _ in 0..5 {
                let d = tokio::task::spawn_blocking({
                    let emu = Arc::clone(&emu);
                    move || emu.lock().unwrap().idle()
                })
                .await??;
                if d > 0 {
                    delay = d as u64;
                    break;
                }
            }
            {
                let mut emu = emu.lock().unwrap();
                if emu.gfx_changed()? {
                    let screen = emu.get_screen()?;
                    let _ = output.send(Output::Screen(Box::new(screen)));
                }
                send_output(emu.handle_io()?);
            }

            let mut first = true;
            loop {
                let timeout =
                    Delay::new(Duration::from_millis(if first { delay.max(10) } else { 1 }));
                first = false;
                select! {
                    _ = timeout => {
                        break;
                    }
                    _ = wake_rx.recv() => {}
                    s = input2_rx.recv() => {
                        if let Some(s) = s {
                            tokio::task::spawn_blocking({
                                let emu = Arc::clone(&emu);
                                move || -> anyhow::Result<()> {
                                    let mut emu = emu.lock().unwrap();
                                    match s {
                                        Input::Console(s) => emu.push_string(&s),
                                        Input::Touch(x, y, on) => emu.send_touch(x, y, on),
                                        Input::Button(on) => emu.press_button(on),
                                    }
                                }
                            }).await??;
                        }
                    }
                }
            }
        }
    }
}
