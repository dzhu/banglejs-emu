use std::{
    borrow::Borrow,
    fmt::Display,
    mem,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use log::{debug, trace};
use wasmtime::{AsContextMut, Caller, Engine, Instance, Linker, Module, Store, TypedFunc};

pub const BTN1: i32 = 17;

#[derive(Clone, Copy, Default, Eq, PartialEq)]
pub struct Color(u8);

impl Color {
    pub fn new(val: u8) -> Self {
        Self(val & 7)
    }

    pub fn fg(&self) -> u8 {
        30 + self.0
    }

    pub fn bg(&self) -> u8 {
        40 + self.0
    }

    pub fn rgb(&self) -> (bool, bool, bool) {
        (self.0 & 1 != 0, self.0 & 2 != 0, self.0 & 4 != 0)
    }
}

pub struct Screen(pub [[Color; 176]; 176]);

impl Default for Screen {
    fn default() -> Self {
        Self([[Default::default(); 176]; 176])
    }
}

impl Display for Screen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for y in (0..176).step_by(2) {
            for x in 0..176 {
                write!(
                    f,
                    "\x1b[{};{}m\u{2584}",
                    self.0[y][x].bg(),
                    self.0[y + 1][x].fg()
                )?;
            }
            writeln!(f, "\x1b[m")?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum Input {
    Console(Vec<u8>),
    Touch(u8, u8, bool),
    Button(bool),
}

pub enum Output {
    Console(Vec<u8>),
    Screen(Box<Screen>),
}

struct State {
    pins: Vec<bool>,
    flash: Vec<u8>,
    char_q: Vec<u8>,
    instance: Option<Instance>,
}

impl State {
    fn init_banglejs2() -> Self {
        let mut pins = vec![false; 48];
        pins[BTN1 as usize] = true;

        Self {
            pins,
            flash: vec![255u8; 1 << 23],
            instance: None,
            char_q: vec![],
        }
    }
}

struct ModuleFuncs {
    get_gfx_ptr: TypedFunc<i32, i32>,
    js_gfx_changed: TypedFunc<(), i32>,
    js_idle: TypedFunc<(), i32>,
    js_init: TypedFunc<(), ()>,
    js_push_char: TypedFunc<(i32, i32), ()>,
    js_reset_storage: TypedFunc<(), ()>,
    js_send_pin_watch_event: TypedFunc<i32, ()>,
    js_send_touch_event: TypedFunc<(i32, i32, i32, i32), ()>,
}

#[repr(u8)]
enum Gesture {
    Drag = 0,
    Down = 1,
    Up = 2,
    Left = 3,
    Right = 4,
    Touch = 5,
}

#[derive(Debug, Default)]
struct TouchTracker {
    start_last: Option<((u8, u8), (u8, u8))>,
    dist: (u64, u64),
}

impl TouchTracker {
    fn add_touch(&mut self, pt: (u8, u8), on: bool) -> Vec<Gesture> {
        match (self.start_last, on) {
            // Start new touch -- record start and emit a drag.
            (None, true) => {
                self.start_last = Some((pt, pt));
                self.dist = (0, 0);
                vec![Gesture::Drag]
            }
            // Continue existing touch -- update state and emit a drag.
            (Some((start, last)), true) => {
                self.dist.0 += u64::from(pt.0.abs_diff(last.0));
                self.dist.1 += u64::from(pt.1.abs_diff(last.1));
                self.start_last = Some((start, pt));
                vec![Gesture::Drag]
            }
            // Release existing touch -- check stats and see what to emit in
            // addition to a drag.
            (Some((start, last)), false) => {
                self.dist.0 += u64::from(pt.0.abs_diff(last.0));
                self.dist.1 += u64::from(pt.1.abs_diff(last.1));

                let mut ret = vec![Gesture::Drag];

                if self.dist.0 < 5 && self.dist.1 < 5 {
                    ret.push(Gesture::Touch);
                }
                if self.dist.0 > 80 && self.dist.1 < 20 {
                    ret.push(if pt.0 > start.0 {
                        Gesture::Right
                    } else {
                        Gesture::Left
                    });
                }
                if self.dist.0 < 20 && self.dist.1 > 80 {
                    ret.push(if pt.1 > start.1 {
                        Gesture::Down
                    } else {
                        Gesture::Up
                    });
                }

                self.start_last = None;
                ret
            }
            // Supposedly end touch when already ended -- ignore.
            (None, false) => vec![],
        }
    }
}

pub struct Emulator {
    store: Store<State>,
    instance: Instance,
    funcs: ModuleFuncs,

    touch: TouchTracker,
}

impl Emulator {
    pub fn new<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let engine = Engine::default();

        let mut linker = Linker::new(&engine);

        linker.func_wrap("env", "jsHandleIO", |mut caller: Caller<'_, State>| {
            let instance = caller.data().instance.unwrap();
            let mut char_q = mem::take(&mut caller.data_mut().char_q);
            Self::js_handle_io(&mut caller, &instance, &mut char_q).unwrap();
            caller.data_mut().char_q = char_q;
        })?;

        linker.func_wrap(
            "env",
            "hwFlashRead",
            |caller: Caller<'_, State>, ind: i32| -> i32 {
                trace!("hwFlashRead {ind}");
                caller.data().flash[ind as usize] as i32
            },
        )?;

        linker.func_wrap(
            "env",
            "hwFlashWritePtr",
            |mut caller: Caller<'_, State>, flash_addr: i32, base: i32, len: i32| {
                debug!("hwFlashWritePtr {flash_addr} {base} {len}");
                let memory = caller.get_export("memory").unwrap().into_memory().unwrap();
                let mut flash = mem::take(&mut caller.data_mut().flash);
                let dst = &mut flash[flash_addr as usize..][..len as usize];
                memory.read(&caller, base as usize, dst).unwrap();
                debug!("writing at {flash_addr}: {dst:?}");
                caller.data_mut().flash = flash;
            },
        )?;

        linker.func_wrap(
            "env",
            "hwGetPinValue",
            |caller: Caller<'_, State>, ind: i32| -> i32 {
                debug!("hwGetPinValue {ind}");
                caller.data().pins[ind as usize] as i32
            },
        )?;

        linker.func_wrap(
            "env",
            "hwSetPinValue",
            |mut caller: Caller<'_, State>, ind: i32, val: i32| {
                debug!("hwSetPinValue {ind} {val}");
                caller.data_mut().pins[ind as usize] = val != 0
            },
        )?;

        linker.func_wrap("env", "nowMillis", || -> f64 {
            trace!("nowMillis");
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs_f64()
                * 1000.0
        })?;

        let mut store = Store::new(&engine, State::init_banglejs2());
        let module = Module::from_file(&engine, path)?;
        let instance = linker.instantiate(&mut store, &module)?;

        store.data_mut().instance = Some(instance);

        let funcs = ModuleFuncs {
            get_gfx_ptr: instance.get_typed_func(&mut store, "jsGfxGetPtr")?,
            js_gfx_changed: instance.get_typed_func(&mut store, "jsGfxChanged")?,
            js_idle: instance.get_typed_func(&mut store, "jsIdle")?,
            js_init: instance.get_typed_func(&mut store, "jsInit")?,
            js_push_char: instance.get_typed_func(&mut store, "jshPushIOCharEvent")?,
            js_reset_storage: instance.get_typed_func(&mut store, "jsfResetStorage")?,
            js_send_pin_watch_event: instance.get_typed_func(&mut store, "jsSendPinWatchEvent")?,
            js_send_touch_event: instance.get_typed_func(&mut store, "jsSendTouchEvent")?,
        };
        Ok(Self {
            store,
            instance,
            funcs,
            touch: Default::default(),
        })
    }

    pub fn new_with_flash<P: AsRef<Path>>(path: P, data: &[u8]) -> anyhow::Result<Self> {
        let mut emu = Self::new(path)?;
        let flash = &mut emu.store.data_mut().flash;
        let n = flash.len().min(data.len());
        flash[..n].copy_from_slice(&data[..n]);
        Ok(emu)
    }

    pub fn init(&mut self) -> anyhow::Result<()> {
        self.funcs.js_init.call(&mut self.store, ())
    }

    pub fn idle(&mut self) -> anyhow::Result<i32> {
        self.funcs.js_idle.call(&mut self.store, ())
    }

    pub fn gfx_changed(&mut self) -> anyhow::Result<bool> {
        Ok(self.funcs.js_gfx_changed.call(&mut self.store, ())? != 0)
    }

    fn js_handle_io(
        context: &mut impl AsContextMut<Data = State>,
        instance: &Instance,
        char_q: &mut Vec<u8>,
    ) -> anyhow::Result<()> {
        trace!("jsHandleIO");
        let mut context = context.as_context_mut();
        let get_device =
            instance.get_typed_func::<(), i32>(&mut context, "jshGetDeviceToTransmit")?;
        let get_char = instance.get_typed_func::<i32, i32>(&mut context, "jshGetCharToTransmit")?;

        loop {
            let device = get_device.call(&mut context, ()).unwrap();
            if device == 0 {
                break Ok(());
            }
            let ch = get_char.call(&mut context, device)?;
            if let Ok(ch) = ch.try_into() {
                char_q.push(ch);
            } else {
                return Ok(());
            }
        }
    }

    pub fn handle_io(&mut self) -> anyhow::Result<Vec<u8>> {
        let mut char_q = mem::take(&mut self.store.data_mut().char_q);
        Self::js_handle_io(&mut self.store, &self.instance, &mut char_q)?;
        Ok(char_q)
    }

    pub fn reset_storage(&mut self) -> anyhow::Result<()> {
        self.funcs.js_reset_storage.call(&mut self.store, ())
    }

    pub fn get_screen(&mut self) -> anyhow::Result<Screen> {
        let memory = self
            .instance
            .get_memory(&mut self.store, "memory")
            .ok_or(anyhow::format_err!("failed to find `memory` export"))?;

        let mut screen = Screen::default();

        let mut buf = vec![0u8; 66];

        for y in 0..176 {
            let base = self.funcs.get_gfx_ptr.call(&mut self.store, y as i32)?;
            memory.read(&self.store, base as usize, &mut buf)?;

            fn get3(x: usize, buf: &[u8]) -> u8 {
                let bit = x * 3;
                let byte = bit >> 3;
                ((buf[byte] >> (bit & 7))
                    | if (bit & 7) <= 5 {
                        0
                    } else {
                        buf[byte + 1] << (8 - (bit & 7))
                    })
                    & 7
            }

            for x in 0..176 {
                screen.0[y][x] = Color::new(get3(x, &buf));
            }
        }
        Ok(screen)
    }

    pub fn push_string<T, B>(&mut self, chars: T) -> anyhow::Result<()>
    where
        B: Borrow<u8>,
        T: IntoIterator<Item = B>,
    {
        for ch in chars.into_iter() {
            self.funcs
                .js_push_char
                .call(&mut self.store, (21, *ch.borrow() as i32))?;
            self.idle()?;
        }

        Ok(())
    }

    pub fn send_pin_watch_event(&mut self, pin: i32) -> anyhow::Result<()> {
        self.funcs
            .js_send_pin_watch_event
            .call(&mut self.store, pin)
    }

    pub fn send_touch(&mut self, x: u8, y: u8, on: bool) -> anyhow::Result<()> {
        for gesture in self.touch.add_touch((x, y), on) {
            self.funcs.js_send_touch_event.call(
                &mut self.store,
                (x as i32, y as i32, on as i32, gesture as i32),
            )?;
        }
        Ok(())
    }

    pub fn press_button(&mut self, on: bool) -> anyhow::Result<()> {
        // Pin values are expected to be inverted.
        self.store.data_mut().pins[BTN1 as usize] = !on;
        self.send_pin_watch_event(BTN1)
    }
}
