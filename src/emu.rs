use std::{
    borrow::Borrow,
    collections::VecDeque,
    mem,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use log::{debug, trace};
use wasmtime::{AsContextMut, Caller, Engine, Instance, Linker, Module, Store, TypedFunc};

pub const BTN1: i32 = 17;

struct State {
    pins: Vec<bool>,
    flash: Vec<u8>,
    char_q: VecDeque<u8>,
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
            char_q: VecDeque::new(),
        }
    }
}

struct ModuleFuncs {
    get_gfx_ptr: TypedFunc<i32, i32>,
    js_gfx_changed: TypedFunc<(), i32>,
    js_idle: TypedFunc<(), i32>,
    js_init: TypedFunc<(), ()>,
    js_push_char: TypedFunc<(i32, i32), ()>,
    js_send_pin_watch_event: TypedFunc<i32, ()>,
}

pub struct Emulator {
    store: Store<State>,
    instance: Instance,
    funcs: ModuleFuncs,
}

impl Emulator {
    pub fn new<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let engine = Engine::default();

        let mut linker = Linker::new(&engine);

        linker.func_wrap("env", "jsHandleIO", |mut caller: Caller<'_, State>| {
            let instance = caller.data().instance.unwrap();
            let mut char_q = mem::take(&mut caller.data_mut().char_q);
            Self::js_handle_io(&mut caller, &instance, |ch| char_q.push_back(ch)).unwrap();
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
            js_send_pin_watch_event: instance.get_typed_func(&mut store, "jsSendPinWatchEvent")?,
        };
        Ok(Self {
            store,
            instance,
            funcs,
        })
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
        mut cb: impl FnMut(u8),
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
                cb(ch);
            } else {
                return Ok(());
            }
        }
    }

    pub fn handle_io(&mut self, mut cb: impl FnMut(u8)) -> anyhow::Result<()> {
        for ch in mem::take(&mut self.store.data_mut().char_q) {
            cb(ch);
        }
        Self::js_handle_io(&mut self.store, &self.instance, cb)?;
        Ok(())
    }

    pub fn draw_screen(&mut self) -> anyhow::Result<()> {
        let memory = self
            .instance
            .get_memory(&mut self.store, "memory")
            .ok_or(anyhow::format_err!("failed to find `memory` export"))?;

        let mut buf0 = vec![0u8; 66];
        let mut buf1 = vec![0u8; 66];

        for y in (0..176).step_by(2) {
            let base0 = self.funcs.get_gfx_ptr.call(&mut self.store, y)?;
            let base1 = self.funcs.get_gfx_ptr.call(&mut self.store, y + 1)?;
            memory.read(&self.store, base0 as usize, &mut buf0)?;
            memory.read(&self.store, base1 as usize, &mut buf1)?;

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
                let c0 = get3(x, &buf0);
                let c1 = get3(x, &buf1);
                print!("\x1b[{};{}m\u{2584}", 40 + c0, 30 + c1);
            }
            println!("\x1b[m");
        }
        Ok(())
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
}
