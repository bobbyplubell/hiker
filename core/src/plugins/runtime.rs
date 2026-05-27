//! The WASM execution seam. [`WasmEngine`] hides the concrete runtime so it
//! can be swapped (the spec's eventual wasmtime/Component-Model path) without
//! touching the host; [`WasmiEngine`] is the v1 pure-Rust interpreter impl,
//! the only place `wasmi` is referenced (`plugin-runtime-wasmi`).
//!
//! ABI — a language-agnostic "fat pipe" over linear memory. The plugin
//! exports `memory` + `plugin_alloc(len)->ptr` and entry points
//! `(ptr,len)->i64` (the result's `ptr<<32 | len` in plugin memory); it
//! imports `hiker.host_call(name_ptr,name_len,args_ptr,args_len)->i64`. All
//! payloads are JSON strings, so the surface evolves without ABI churn. Every
//! call runs under a fuel budget so a runaway plugin traps instead of hanging
//! the host (`plugin-resource-limits`).
//
// status: plugin-runtime-wasmi, plugin-host-call

use super::dispatch::HostInvoker;
use super::error::Error;

/// Fuel granted per plugin entry call. Generous for UI/query work; a runaway
/// loop exhausts it and traps rather than blocking the host thread.
const FUEL_PER_CALL: u64 = 2_000_000_000;

/// A swappable WebAssembly runtime. Instantiating binds a plugin's bytes to
/// its [`HostInvoker`] (granted permissions + host API) so every `host_call`
/// the instance makes is gated.
pub trait WasmEngine: Send + Sync {
    /// Instantiate `wasm`, wiring the `hiker.host_call` import to `invoker`.
    fn instantiate(
        &self,
        wasm: &[u8],
        invoker: HostInvoker,
    ) -> Result<Box<dyn PluginInstance>, Error>;
}

/// A live plugin instance. Entry points take and return JSON strings over the
/// fat-pipe ABI; the trait hides the marshalling.
pub trait PluginInstance: Send {
    /// Call the exported entry `export` with `input` (JSON), returning the
    /// plugin's JSON result. `init` is invoked with an empty input.
    fn call_json(&mut self, export: &str, input: &str) -> Result<String, Error>;
}

/// Pure-Rust interpreter engine (`wasmi`). No JIT — small, auditable,
/// matches the clean-SBOM posture in `deny.toml`.
pub struct WasmiEngine;

impl WasmEngine for WasmiEngine {
    fn instantiate(
        &self,
        wasm: &[u8],
        invoker: HostInvoker,
    ) -> Result<Box<dyn PluginInstance>, Error> {
        let mut config = wasmi::Config::default();
        config.consume_fuel(true);
        let engine = wasmi::Engine::new(&config);
        let module =
            wasmi::Module::new(&engine, wasm).map_err(|e| Error::Engine(e.to_string()))?;
        let mut store = wasmi::Store::new(&engine, invoker);
        store
            .set_fuel(FUEL_PER_CALL)
            .map_err(|e| Error::Engine(e.to_string()))?;
        let mut linker = wasmi::Linker::<HostInvoker>::new(&engine);
        linker
            .func_wrap("hiker", "host_call", host_call_shim)
            .map_err(|e| Error::Engine(e.to_string()))?;
        let instance = linker
            .instantiate_and_start(&mut store, &module)
            .map_err(|e| Error::Engine(e.to_string()))?;
        // Fail fast if the plugin doesn't honor the ABI surface.
        if instance.get_memory(&store, "memory").is_none() {
            return Err(Error::Abi("plugin exports no `memory`".to_string()));
        }
        if instance
            .get_typed_func::<i32, i32>(&store, "plugin_alloc")
            .is_err()
        {
            return Err(Error::Abi(
                "plugin exports no `plugin_alloc(i32)->i32`".to_string(),
            ));
        }
        Ok(Box::new(WasmiInstance { store, instance }))
    }
}

struct WasmiInstance {
    store: wasmi::Store<HostInvoker>,
    instance: wasmi::Instance,
}

impl PluginInstance for WasmiInstance {
    fn call_json(&mut self, export: &str, input: &str) -> Result<String, Error> {
        // Refuel each call so one entry's budget can't starve the next.
        self.store
            .set_fuel(FUEL_PER_CALL)
            .map_err(|e| Error::Engine(e.to_string()))?;
        let memory = self
            .instance
            .get_memory(&self.store, "memory")
            .ok_or_else(|| Error::Abi("no `memory` export".to_string()))?;
        let alloc = self
            .instance
            .get_typed_func::<i32, i32>(&self.store, "plugin_alloc")
            .map_err(|e| Error::Abi(e.to_string()))?;
        let in_len = i32::try_from(input.len())
            .map_err(|_| Error::Abi("input exceeds i32".to_string()))?;
        let in_ptr = alloc
            .call(&mut self.store, in_len)
            .map_err(|e| Error::Trap(e.to_string()))?;
        memory
            .write(&mut self.store, in_ptr as usize, input.as_bytes())
            .map_err(|e| Error::Abi(e.to_string()))?;
        let entry = self
            .instance
            .get_typed_func::<(i32, i32), i64>(&self.store, export)
            .map_err(|e| Error::Abi(e.to_string()))?;
        let packed = entry
            .call(&mut self.store, (in_ptr, in_len))
            .map_err(|e| Error::Trap(e.to_string()))?;
        read_packed(memory, &self.store, packed)
    }
}

/// Decode a `ptr<<32 | len` result and read that slice of plugin memory as a
/// UTF-8 string. `memory` is a `wasmi::Memory` handle (Copy), passed by value.
fn read_packed(
    memory: wasmi::Memory,
    store: &wasmi::Store<HostInvoker>,
    packed: i64,
) -> Result<String, Error> {
    let ptr = ((packed >> 32) & 0xffff_ffff) as usize;
    let len = (packed & 0xffff_ffff) as usize;
    let mut buf = vec![0u8; len];
    memory
        .read(store, ptr, &mut buf)
        .map_err(|e| Error::Abi(e.to_string()))?;
    String::from_utf8(buf).map_err(|e| Error::Abi(e.to_string()))
}

/// The `hiker.host_call` import: read `name`/`args` from plugin memory, run
/// them through the instance's [`HostInvoker`] gate, write the JSON result
/// back into plugin memory (via the plugin's `plugin_alloc`), and return its
/// packed `ptr<<32 | len`. Any marshalling failure returns `0` (empty result),
/// which the plugin treats as an error.
fn host_call_shim(
    mut caller: wasmi::Caller<'_, HostInvoker>,
    name_ptr: i32,
    name_len: i32,
    args_ptr: i32,
    args_len: i32,
) -> i64 {
    let Some(memory) = caller.get_export("memory").and_then(wasmi::Extern::into_memory) else {
        return 0;
    };
    let Some(name) = read_string(memory, &caller, name_ptr, name_len) else {
        return 0;
    };
    let Some(args) = read_string(memory, &caller, args_ptr, args_len) else {
        return 0;
    };
    // Gate + dispatch. A refusal is delivered to the plugin as a JSON error
    // object rather than a silent failure.
    let payload = match caller.data().invoke(&name, &args) {
        Ok(json) => json,
        Err(message) => error_json(&message),
    };
    write_result(&mut caller, memory, &payload)
}

/// Read `len` bytes at `ptr` from plugin memory as a UTF-8 string.
fn read_string(
    memory: wasmi::Memory,
    caller: &wasmi::Caller<'_, HostInvoker>,
    ptr: i32,
    len: i32,
) -> Option<String> {
    let mut buf = vec![0u8; len.max(0) as usize];
    memory.read(caller, ptr as usize, &mut buf).ok()?;
    String::from_utf8(buf).ok()
}

/// Allocate space in plugin memory via its `plugin_alloc`, write `payload`,
/// return the packed `ptr<<32 | len`. `0` on any failure.
fn write_result(
    caller: &mut wasmi::Caller<'_, HostInvoker>,
    memory: wasmi::Memory,
    payload: &str,
) -> i64 {
    let Ok(len) = i32::try_from(payload.len()) else {
        return 0;
    };
    let Some(alloc) = caller
        .get_export("plugin_alloc")
        .and_then(wasmi::Extern::into_func)
    else {
        return 0;
    };
    let Ok(alloc) = alloc.typed::<i32, i32>(&caller) else {
        return 0;
    };
    let Ok(ptr) = alloc.call(&mut *caller, len) else {
        return 0;
    };
    if memory.write(&mut *caller, ptr as usize, payload.as_bytes()).is_err() {
        return 0;
    }
    (i64::from(ptr) << 32) | i64::from(len)
}

/// Wrap an error message as the `{ "error": "..." }` JSON the plugin sees.
fn error_json(message: &str) -> String {
    serde_json::json!({ "error": message }).to_string()
}
