use std::{
    env, fs,
    ops::{Deref, DerefMut},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use wasmer::{
    Extern, Function, FunctionEnv, FunctionType, Instance, Memory, MemoryType, Module, Pages,
    RuntimeError, Store, Type, TypedFunction, Value,
};
use wasmer_wasi::{import_object_for_all_wasi_versions, WasiState};

const BTN1: usize = 17;

fn main() -> anyhow::Result<()> {
    let wasm_bytes = fs::read(env::args().nth(1).unwrap()).unwrap();
    let store_arc = Arc::new(Mutex::new(Store::default()));
    let mut store = store_arc.lock().unwrap();
    let store = store.deref_mut();
    let module = Module::new(store, wasm_bytes)?;

    let mut wasi_state_builder = WasiState::new("espruino");

    let wasi_env = wasi_state_builder.finalize(store)?;
    let mut import_object = import_object_for_all_wasi_versions(store, &wasi_env.env);

    let flash = Arc::new(Mutex::new(vec![255u8; 1 << 23]));
    let pins = Arc::new(Mutex::new(vec![false; 48]));

    pins.lock().unwrap()[BTN1] = true;

    let env_name = |s: &str| ("env".to_owned(), s.to_owned());

    #[derive(Clone, Debug)]
    struct Env {
        instance: Arc<Mutex<Option<Instance>>>,
    }
    let instance_env = FunctionEnv::new(
        store,
        Env {
            instance: Arc::new(Mutex::new(None)),
        },
    );

    fn js_handle_io(store: &mut Store, instance: &Instance) -> anyhow::Result<()> {
        let get_device: TypedFunction<(), i32> = instance
            .exports
            .get_typed_function(store, "jshGetDeviceToTransmit")?;
        let get_char: TypedFunction<i32, i32> = instance
            .exports
            .get_typed_function(store, "jshGetCharToTransmit")?;

        loop {
            let device = get_device.call(store)?;
            if device == 0 {
                println!();
                break Ok(());
            }
            let ch = char::from_u32(get_char.call(store, device)? as _).unwrap();
            print!("{ch}");
        }
    }

    import_object.extend([
        (
            env_name("jsHandleIO"),
            Extern::Function(Function::new_with_env(
                store,
                &instance_env,
                FunctionType::new([], []),
                {
                    let store = Arc::clone(&store_arc);
                    move |env, _| {
                        println!("jsHandleIO");

                        let instance = env.data().instance.lock().unwrap();
                        let instance = instance.as_ref().unwrap();
                        let mut store = store.lock().unwrap();
                        let store = store.deref_mut();

                        js_handle_io(store, instance).unwrap();

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
                        println!("hwFlashRead {args:?}");
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
                    let store = Arc::clone(&store_arc);
                    move |env, args| {
                        println!("hwFlashWritePtr {args:?}");
                        let flash_addr = args[0].unwrap_i32();
                        let base = args[1].unwrap_i32();
                        let len = args[2].unwrap_i32();

                        let mut flash = flash.lock().unwrap();
                        let dst = &mut flash[flash_addr as usize..][..len as usize];
                        env.data()
                            .memory_view(store.lock().unwrap().deref())
                            .read(base as u64, dst)
                            .unwrap();
                        println!("{flash_addr} {dst:?}");
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
                        println!("hwGetPinValue {args:?}");
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
                        println!("hwSetPinValue {args:?}");
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
                    println!("nowMillis");
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
    *instance_env.as_mut(store).instance.lock().unwrap() = Some(instance.clone());

    let js_init: TypedFunction<(), ()> = instance.exports.get_typed_function(&store, "jsInit")?;
    let js_idle: TypedFunction<(), i32> = instance.exports.get_typed_function(&store, "jsIdle")?;
    let js_send_pin_watch_event: TypedFunction<i32, ()> = instance
        .exports
        .get_typed_function(&store, "jsSendPinWatchEvent")?;
    let js_gfx_get_ptr: TypedFunction<i32, i32> =
        instance.exports.get_typed_function(&store, "jsGfxGetPtr")?;

    fn draw_screen(
        store: &mut Store,
        memory: &Memory,
        get: TypedFunction<i32, i32>,
    ) -> anyhow::Result<()> {
        let mut buf = vec![0u8; 66];
        let memory_view = memory.view(&store);
        for y in 0..176 {
            let base = get.call(store, y)?;
            memory_view.read(base as u64, &mut buf)?;
            for x in 0..176 {
                let bit = x * 3;
                let byte = bit >> 3;
                let c = ((buf[byte] >> (bit & 7))
                    | if (bit & 7) <= 5 {
                        0
                    } else {
                        buf[byte + 1] << (8 - (bit & 7))
                    })
                    & 7;
                print!("\x1b[{}m ", 40 + c);
            }
            println!("\x1b[m");
        }
        Ok(())
    }

    println!("==== init");
    js_init.call(store)?;
    js_send_pin_watch_event.call(store, BTN1 as i32)?;
    js_handle_io(store, &instance)?;

    draw_screen(store, memory, js_gfx_get_ptr)?;

    for step in 0..10 {
        println!("==== step {step}");
        let ret = js_idle.call(store)?;
        println!("-> {ret:?}");
        js_handle_io(store, &instance)?;
    }

    Ok(())
}
