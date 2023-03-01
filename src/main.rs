use std::{
    borrow::Borrow,
    env, fs,
    ops::DerefMut,
    path::Path,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose, Engine};
use log::{debug, info, trace};
use wasmer::{
    AsStoreMut, Extern, Function, FunctionEnv, FunctionType, Instance, Memory, MemoryType, Module,
    Pages, RuntimeError, Store, Type, TypedFunction, Value,
};
use wasmer_wasi::{import_object_for_all_wasi_versions, WasiState};

const BTN1: usize = 17;

struct Emulator {
    store: Arc<Mutex<Store>>,
    instance: Arc<Mutex<Option<Instance>>>,
}

impl Emulator {
    fn js_handle_io(store: &mut impl AsStoreMut, instance: &Instance) -> anyhow::Result<()> {
        let get_device: TypedFunction<(), i32> = instance
            .exports
            .get_typed_function(store, "jshGetDeviceToTransmit")?;
        let get_char: TypedFunction<i32, i32> = instance
            .exports
            .get_typed_function(store, "jshGetCharToTransmit")?;

        loop {
            let device = get_device.call(store)?;
            if device == 0 {
                break Ok(());
            }
            let ch = char::from_u32(get_char.call(store, device)? as _).unwrap();
            print!("{ch}");
        }
    }

    fn new<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let wasm_bytes = fs::read(path)?;
        let store_arc = Arc::new(Mutex::new(Store::default()));
        let mut store_guard = store_arc.lock().unwrap();
        let store = store_guard.deref_mut();
        let module = Module::new(store, wasm_bytes)?;

        let mut wasi_state_builder = WasiState::new("espruino");

        let wasi_env = wasi_state_builder.finalize(store)?;
        let mut import_object = import_object_for_all_wasi_versions(store, &wasi_env.env);

        let flash = Arc::new(Mutex::new(vec![255u8; 1 << 23]));
        let pins = Arc::new(Mutex::new(vec![false; 48]));

        pins.lock().unwrap()[BTN1] = true;

        let env_name = |s: &str| ("env".to_owned(), s.to_owned());

        let env = FunctionEnv::new(store, ());

        let instance_arc: Arc<Mutex<Option<Instance>>> = Arc::new(Mutex::new(None));

        import_object.extend([
            (
                env_name("jsHandleIO"),
                Extern::Function(Function::new_with_env(
                    store,
                    &env,
                    FunctionType::new([], []),
                    {
                        let instance = Arc::clone(&instance_arc);
                        move |mut env, _| {
                            debug!("jsHandleIO");
                            let instance = instance.lock().unwrap();
                            let instance = instance.as_ref().unwrap();
                            Self::js_handle_io(&mut env, instance).unwrap();

                            Ok(vec![])
                        }
                    },
                )),
            ),
            (
                env_name("hwFlashRead"),
                Extern::Function(Function::new(
                    store,
                    FunctionType::new([Type::I32], [Type::I32]),
                    {
                        let flash = Arc::clone(&flash);
                        move |args| {
                            trace!("hwFlashRead {args:?}");
                            match args[0] {
                                Value::I32(ind) => {
                                    Ok(vec![Value::I32(flash.lock().unwrap()[ind as usize] as i32)])
                                }
                                _ => Err(RuntimeError::new("bad type")),
                            }
                        }
                    },
                )),
            ),
            (
                env_name("hwFlashWritePtr"),
                Extern::Function(Function::new_with_env(
                    store,
                    &wasi_env.env,
                    FunctionType::new([Type::I32, Type::I32, Type::I32], []),
                    {
                        let flash = Arc::clone(&flash);
                        move |env, args| {
                            trace!("hwFlashWritePtr {args:?}");
                            let flash_addr = args[0].unwrap_i32();
                            let base = args[1].unwrap_i32();
                            let len = args[2].unwrap_i32();

                            let mut flash = flash.lock().unwrap();
                            let dst = &mut flash[flash_addr as usize..][..len as usize];
                            env.data().memory_view(&env).read(base as u64, dst).unwrap();
                            trace!("writing at {flash_addr}: {dst:?}");
                            Ok(vec![])
                        }
                    },
                )),
            ),
            (
                env_name("hwGetPinValue"),
                Extern::Function(Function::new(
                    store,
                    FunctionType::new([Type::I32], [Type::I32]),
                    {
                        let pins = Arc::clone(&pins);
                        move |args| {
                            debug!("hwGetPinValue {args:?}");
                            match args[0] {
                                Value::I32(ind) => {
                                    Ok(vec![Value::I32(pins.lock().unwrap()[ind as usize] as i32)])
                                }
                                _ => Err(RuntimeError::new("bad type")),
                            }
                        }
                    },
                )),
            ),
            (
                env_name("hwSetPinValue"),
                Extern::Function(Function::new(
                    store,
                    FunctionType::new([Type::I32, Type::I32], []),
                    {
                        let pins = Arc::clone(&pins);
                        move |args| {
                            debug!("hwSetPinValue {args:?}");
                            match (&args[0], &args[1]) {
                                (Value::I32(ind), Value::I32(val)) => {
                                    pins.lock().unwrap()[*ind as usize] = *val != 0;
                                    Ok(vec![])
                                }
                                _ => Err(RuntimeError::new("bad type")),
                            }
                        }
                    },
                )),
            ),
            (
                env_name("nowMillis"),
                Extern::Function(Function::new(
                    store,
                    FunctionType::new([], [Type::F32]),
                    |_| {
                        trace!("nowMillis");
                        Ok(vec![Value::F32(
                            SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap()
                                .as_secs_f32()
                                * 1000.0,
                        )])
                    },
                )),
            ),
        ]);

        import_object.extend([(
            env_name("memory"),
            Extern::Memory(
                Memory::new(
                    store,
                    MemoryType {
                        minimum: Pages(1 << 2),
                        maximum: Some(Pages(1 << 2)),
                        shared: false,
                    },
                )
                .unwrap(),
            ),
        )]);

        let instance = Instance::new(store, &module, &import_object)?;
        let memory = instance.exports.get_memory("memory")?;
        wasi_env.data_mut(store).set_memory(memory.clone());
        *instance_arc.lock().unwrap() = Some(instance.clone());

        drop(store_guard);

        Ok(Self {
            store: store_arc,
            instance: instance_arc,
        })
    }

    fn run<T, F>(&self, f: F) -> T
    where
        F: FnOnce(&mut Store, &Instance) -> T,
    {
        let mut store = self.store.lock().unwrap();
        let instance = self.instance.lock().unwrap();

        let store = store.deref_mut();
        let instance = instance.as_ref().unwrap();

        f(store, instance)
    }

    fn init(&self) -> anyhow::Result<()> {
        self.run(|store, instance| {
            let js_init: TypedFunction<(), ()> =
                instance.exports.get_typed_function(store, "jsInit")?;
            Ok(js_init.call(store)?)
        })
    }

    fn idle(&self) -> anyhow::Result<i32> {
        self.run(|store, instance| {
            let js_idle: TypedFunction<(), i32> =
                instance.exports.get_typed_function(&store, "jsIdle")?;
            Ok(js_idle.call(store)?)
        })
    }

    fn handle_io(&self) -> anyhow::Result<()> {
        self.run(|store, instance| Self::js_handle_io(store, instance))
    }

    fn draw_screen(&self) -> anyhow::Result<()> {
        self.run(|store, instance| {
            let memory = instance.exports.get_memory("memory")?;

            let get: TypedFunction<i32, i32> =
                instance.exports.get_typed_function(&store, "jsGfxGetPtr")?;

            let mut buf0 = vec![0u8; 66];
            let mut buf1 = vec![0u8; 66];
            let memory_view = memory.view(&store);
            for y in (0..176).step_by(2) {
                let base0 = get.call(store, y)?;
                let base1 = get.call(store, y + 1)?;
                memory_view.read(base0 as u64, &mut buf0)?;
                memory_view.read(base1 as u64, &mut buf1)?;

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
        })
    }

    fn push_string<T, B>(&self, chars: T) -> anyhow::Result<()>
    where
        B: Borrow<u8>,
        T: IntoIterator<Item = B>,
    {
        self.run(|store, instance| {
            let js_push_char: TypedFunction<(i32, i32), ()> = instance
                .exports
                .get_typed_function(&store, "jshPushIOCharEvent")?;
            let js_idle: TypedFunction<(), i32> =
                instance.exports.get_typed_function(&store, "jsIdle")?;

            for (i, ch) in chars.into_iter().enumerate() {
                js_push_char.call(store, 21, *ch.borrow() as i32)?;
                if (i + 1) % 40 == 0 {
                    Self::js_handle_io(store, instance)?;
                    js_idle.call(store)?;
                }
            }

            Ok(())
        })
    }
}

fn main() -> anyhow::Result<()> {
    env_logger::init();

    let emu = Emulator::new(env::args().nth(1).unwrap())?;

    // let js_send_pin_watch_event: TypedFunction<i32, ()> = instance
    //     .exports
    //     .get_typed_function(&store, "jsSendPinWatchEvent")?;

    info!("==== init");
    emu.init()?;

    // js_send_pin_watch_event.call(store, BTN1 as i32)?;

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

    for step in 0..2 {
        info!("==== step {step}");
        let ret = emu.idle()?;
        info!("-> {ret:?}");
        emu.handle_io()?;
    }

    emu.draw_screen()?;

    Ok(())
}
