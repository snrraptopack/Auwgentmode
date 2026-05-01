use rquickjs::{Context, Function, Object, Runtime, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::types::{
    ExecutionResult, SandboxSnapshot, ToolCall, ToolDefinition, ToolResult,
};

// ─── Error wrapper ────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum JsError {
    #[error("QuickJS error: {0}")]
    Js(#[from] rquickjs::Error),
    #[error("Sandbox error: {0}")]
    Sandbox(String),
}

pub type JsResult<T> = Result<T, JsError>;

// ─── Shared state ─────────────────────────────────────────────────────────────

struct SharedState {
    next_id: u64,
    /// Staging map: populated by `__auwgent_call_*` before the Promise is created.
    staged: HashMap<u64, ToolCall>,
    /// Live calls awaiting host resolution: populated during `drain_resolvers()`.
    pending: HashMap<u64, (ToolCall, rquickjs::Persistent<Function<'static>>)>,
    console: String,
}

impl SharedState {
    fn new() -> Self {
        Self {
            next_id: 0,
            staged: HashMap::new(),
            pending: HashMap::new(),
            console: String::new(),
        }
    }

    fn clear_for_execute(&mut self) {
        self.staged.clear();
        self.pending.clear();
        self.console.clear();
    }
}

// ─── QuickJsSandbox ──────────────────────────────────────────────────────────

pub struct QuickJsSandbox {
    // !! Drop order matters: state → context → runtime !!
    // Rust drops fields in declaration order (top → bottom).
    // `state` holds `Persistent<Function>` handles that must be released
    // before the `Context` (which contains the JS heap) is destroyed.
    // `Context` must be destroyed before `Runtime`.
    state: Arc<Mutex<SharedState>>,
    context: Context,
    runtime: Runtime,

    snapshot_script: Option<String>,
    snapshot_rounds: Vec<Vec<serde_json::Value>>,
    snapshot_tools: Vec<ToolDefinition>,
    snapshot_globals: serde_json::Value,
    snapshot_libraries: Vec<String>,
}

impl QuickJsSandbox {
    /// Create a new QuickJS sandbox with a 20 MB memory cap and infinite-loop protection.
    ///
    /// # Promise / tool-call bridge design
    ///
    /// rquickjs has a fundamental lifetime constraint: `Persistent::save(&ctx, value)`
    /// requires `ctx` and `value` to share the **exact same** `'js` lifetime (both are
    /// invariant). When they arrive as separate closure parameters inside `Function::new`,
    /// Rust infers two independent lifetimes and rejects the call.
    ///
    /// We solve this with a **JS-side resolver queue**:
    ///
    /// 1. Each tool stub (`async function <name>(args)`) calls `__auwgent_call_<name>`
    ///    which records the payload in `SharedState::staged` and returns a unique call ID.
    ///
    /// 2. The stub then creates `new Promise(r => __auwgent_queue.push({id, r}))`.
    ///    The executor pushes `{id, resolve}` into a JS-side global array
    ///    `__auwgent_queue` synchronously during Promise construction.
    ///
    /// 3. After the microtask queue drains (in `drive_to_yield`), Rust calls
    ///    `drain_resolvers` inside a **single `context.with` block**, reading all
    ///    entries from `__auwgent_queue`. Because both `ctx` and each `resolve`
    ///    come from the same `with` closure, they share the same `'js` lifetime —
    ///    `Persistent::save(&ctx, resolve)` compiles without issues.
    ///
    /// 4. `drive_to_yield` then promotes the staged entries to `pending` (matching
    ///    them by ID) and returns `YieldedForTools` with the collected ToolCalls.
    pub fn new() -> JsResult<Self> {
        let runtime = Runtime::new()?;
        runtime.set_memory_limit(20 * 1024 * 1024);

        // Infinite-loop protection (~100 k interrupt ticks, same budget as Luau).
        let interrupt_count = Arc::new(Mutex::new(0u64));
        runtime.set_interrupt_handler(Some({
            let count = interrupt_count.clone();
            Box::new(move || {
                let mut c = count.lock().unwrap();
                *c += 1;
                *c > 100_000
            })
        }));

        let context = Context::full(&runtime)?;
        let state = Arc::new(Mutex::new(SharedState::new()));

        // Install console.log + the global resolver queue.
        {
            let s = state.clone();
            context.with(|ctx| -> rquickjs::Result<()> {
                // ── console.log ────────────────────────────────────────────────
                let console = Object::new(ctx.clone())?;
                let log_fn = Function::new(ctx.clone(), {
                    move |args: rquickjs::function::Rest<Value>| {
                        let mut st = s.lock().unwrap();
                        let line =
                            args.0.iter().map(js_val_to_string).collect::<Vec<_>>().join(" ");
                        st.console.push_str(&line);
                        st.console.push('\n');
                        Ok::<(), rquickjs::Error>(())
                    }
                })?;
                console.set("log", log_fn)?;
                ctx.globals().set("console", console)?;

                // ── JS-side resolver queue ─────────────────────────────────────
                // `__auwgent_queue` is a plain JS array. Tool stub executors push
                // { id: Number, r: Function } objects into it synchronously during
                // Promise construction. Rust drains it in `drain_resolvers`.
                ctx.eval::<(), _>(b"var __auwgent_queue = [];")?;

                Ok(())
            })?;
        }

        Ok(Self {
            runtime,
            context,
            state,
            snapshot_script: None,
            snapshot_rounds: Vec::new(),
            snapshot_tools: Vec::new(),
            snapshot_globals: serde_json::Value::Object(serde_json::Map::new()),
            snapshot_libraries: Vec::new(),
        })
    }

    // ─── Configuration ────────────────────────────────────────────────────────

    /// Inject read-only context variables into the JS global scope.
    pub fn inject_globals(&mut self, ctx_val: serde_json::Value) -> JsResult<()> {
        if let (serde_json::Value::Object(stored), serde_json::Value::Object(incoming)) =
            (&mut self.snapshot_globals, &ctx_val)
        {
            for (k, v) in incoming {
                stored.insert(k.clone(), v.clone());
            }
        }
        self.context.with(|ctx| -> rquickjs::Result<()> {
            if let serde_json::Value::Object(map) = &ctx_val {
                let globals = ctx.globals();
                for (key, val) in map {
                    globals.set(key.as_str(), json_to_js(&ctx, val)?)?;
                }
            }
            Ok(())
        })?;
        Ok(())
    }

    /// Register tool definitions, generating async JS stubs.
    ///
    /// Each tool installs:
    /// - `__auwgent_call_<name>(json_str) → id` — a Rust fn that records the
    ///   payload in `SharedState::staged` and returns a unique call ID.
    /// - `async function <name>(args?)` — a JS stub that calls `__auwgent_call_*`,
    ///   creates a Promise, and pushes `{id, r: resolve}` into `__auwgent_queue`.
    pub fn register_tools(&mut self, tools: &[ToolDefinition]) -> JsResult<()> {
        self.snapshot_tools.extend_from_slice(tools);

        let state = self.state.clone();
        self.context.with(|ctx| -> rquickjs::Result<()> {
            let globals = ctx.globals();

            for tool in tools {
                let tool_name = tool.name.clone();
                let has_args  = tool.has_args;

                // ── Rust call_fn: records payload, issues ID ──────────────────
                let call_fn_name = format!("__auwgent_call_{}", tool_name);
                {
                    let st   = state.clone();
                    let name = tool_name.clone();
                    let call_fn = Function::new(ctx.clone(), move |payload_str: rquickjs::String| {
                        let payload: serde_json::Value =
                            serde_json::from_str(&payload_str.to_string().unwrap_or_default())
                                .unwrap_or(serde_json::Value::Null);
                        let call = ToolCall { tool_name: name.clone(), payload };
                        let mut locked = st.lock().unwrap();
                        locked.next_id += 1;
                        let id = locked.next_id;
                        locked.staged.insert(id, call);
                        Ok::<u64, rquickjs::Error>(id)
                    })?;
                    globals.set(call_fn_name.as_str(), call_fn)?;
                }

                // ── JS async stub ─────────────────────────────────────────────
                // The stub:
                //   1. Serializes args to JSON and passes to call_fn → gets id.
                //   2. Creates a Promise whose executor synchronously pushes
                //      { id, r: resolve } into __auwgent_queue.
                //   3. Returns (awaits) that Promise.
                //
                // Why push to __auwgent_queue instead of calling a Rust fn?
                // Because Persistent::save requires ctx and the function to share
                // the same 'js lifetime. By queuing in JS and draining from a
                // single context.with block in Rust, both come from the same ctx.
                let js_stub = if has_args {
                    format!(
                        r"async function {n}(args) {{
                            const _id = {call}(JSON.stringify(args ?? null));
                            return new Promise(r => __auwgent_queue.push({{ id: _id, r }}));
                        }}",
                        n = tool_name, call = call_fn_name,
                    )
                } else {
                    format!(
                        r"async function {n}() {{
                            const _id = {call}(JSON.stringify(null));
                            return new Promise(r => __auwgent_queue.push({{ id: _id, r }}));
                        }}",
                        n = tool_name, call = call_fn_name,
                    )
                };
                ctx.eval::<(), _>(js_stub.as_bytes())?;
            }
            Ok(())
        })?;
        Ok(())
    }

    /// Load reusable JS library code before execution.
    pub fn load_library(&mut self, js_code: &str) -> JsResult<()> {
        self.snapshot_libraries.push(js_code.to_string());
        self.context.with(|ctx| -> rquickjs::Result<()> {
            ctx.eval::<(), _>(js_code)?;
            Ok(())
        })?;
        Ok(())
    }

    /// Generate a system prompt describing all registered tools.
    pub fn generate_tool_prompt(tools: &[ToolDefinition]) -> String {
        let mut out = String::from("You have access to the following tools:\n\n");
        for t in tools {
            out.push_str(&format!("- `{}`: {}", t.name, t.description));
            if t.has_args {
                if let Some(s) = &t.arg_schema {
                    let schema_str = format!(" Args: `{s}`");
                    out.push_str(&schema_str);
                } else {
                    out.push_str(" Args: `{ ... }`");
                }
            } else {
                out.push_str(" (no arguments)");
            }
            out.push('\n');
        }
        out
    }

    // ─── Execution API ────────────────────────────────────────────────────────

    pub fn get_console_output(&self) -> String {
        self.state.lock().unwrap().console.clone()
    }

    /// Execute a JS script. Wraps it in an async IIFE for top-level `await` support.
    pub fn execute(&mut self, source: &str) -> JsResult<ExecutionResult> {
        self.snapshot_script = Some(source.to_string());
        self.snapshot_rounds.clear();
        self.state.lock().unwrap().clear_for_execute();

        // Reset the queue so stale entries from a previous run don't bleed in.
        self.context.with(|ctx| -> rquickjs::Result<()> {
            ctx.eval::<(), _>(b"__auwgent_queue = [];")?;
            Ok(())
        })?;

        let wrapped = format!("(async () => {{\n{}\n}})();", source);
        self.context.with(|ctx| -> rquickjs::Result<()> {
            ctx.eval::<Value, _>(wrapped.as_bytes())?;
            Ok(())
        })?;

        self.drive_to_yield()
    }

    /// Resume by resolving all pending Promises with JSON values.
    pub fn resume_with_json(&mut self, next_values: Vec<serde_json::Value>) -> JsResult<ExecutionResult> {
        self.snapshot_rounds.push(next_values.clone());

        // Sort IDs ascending so they match the order tools were yielded.
        let ids: Vec<u64> = {
            let st = self.state.lock().unwrap();
            let mut ids: Vec<u64> = st.pending.keys().copied().collect();
            ids.sort_unstable();
            ids
        };

        self.context.with(|ctx| -> rquickjs::Result<()> {
            for (idx, id) in ids.iter().enumerate() {
                let maybe_resolve = {
                    let mut st = self.state.lock().unwrap();
                    st.pending.remove(id).map(|(_, r)| r)
                };
                if let Some(persistent) = maybe_resolve {
                    let resolve = persistent.restore(&ctx)?;
                    let val = next_values
                        .get(idx)
                        .map(|v| json_to_js(&ctx, v))
                        .transpose()?
                        .unwrap_or_else(|| Value::new_undefined(ctx.clone()));
                    resolve.call::<_, ()>((val,))?;
                }
            }
            Ok(())
        })?;

        self.drive_to_yield()
    }

    /// Resume with structured per-tool results (supports per-tool errors).
    ///
    /// `ToolResult::Err` becomes `{ __error: true, message: "..." }` —
    /// same sentinel shape as the Luau engine for cross-engine host compatibility.
    pub fn resume_with_results(&mut self, results: Vec<ToolResult>) -> JsResult<ExecutionResult> {
        let json_vals = results
            .into_iter()
            .map(|r| match r {
                ToolResult::Ok(v)  => v,
                ToolResult::Err(m) => serde_json::json!({ "__error": true, "message": m }),
            })
            .collect();
        self.resume_with_json(json_vals)
    }

    // ─── Snapshot API ─────────────────────────────────────────────────────────

    pub fn snapshot(&self) -> Option<SandboxSnapshot> {
        Some(SandboxSnapshot {
            script_source: self.snapshot_script.clone()?,
            completed_tool_results: self.snapshot_rounds.clone(),
            tool_definitions: self.snapshot_tools.clone(),
            injected_globals: self.snapshot_globals.clone(),
            libraries: self.snapshot_libraries.clone(),
        })
    }

    pub fn from_snapshot(snapshot: SandboxSnapshot) -> JsResult<(Self, ExecutionResult)> {
        let mut engine = QuickJsSandbox::new()?;
        for lib in &snapshot.libraries {
            engine.load_library(lib)?;
        }
        engine.register_tools(&snapshot.tool_definitions)?;
        engine.inject_globals(snapshot.injected_globals.clone())?;
        let mut status = engine.execute(&snapshot.script_source)?;
        for cached_round in snapshot.completed_tool_results {
            match status {
                ExecutionResult::YieldedForTools { .. } => {
                    status = engine.resume_with_json(cached_round)?;
                }
                ExecutionResult::Finished { .. } | ExecutionResult::Error(_) => break,
            }
        }
        Ok((engine, status))
    }

    // ─── Internal ─────────────────────────────────────────────────────────────

    /// Drain the JS-side `__auwgent_queue` into `SharedState::pending`.
    ///
    /// Each entry in the queue is `{ id: Number, r: Function }`.
    /// We read them all in a single `context.with` closure so that `ctx` and
    /// each `resolve` function share the same `'js` lifetime — the prerequisite
    /// for `Persistent::save` to compile without lifetime errors.
    fn drain_resolvers(&mut self) -> JsResult<()> {
        self.context.with(|ctx| -> rquickjs::Result<()> {
            let globals = ctx.globals();
            let queue: rquickjs::Array = globals.get("__auwgent_queue")?;
            let len = queue.len();

            for i in 0..len {
                let entry: Object = queue.get(i)?;
                let id: u64 = entry.get("id")?;
                let resolve: Function = entry.get("r")?;

                // Both `ctx` and `resolve` come from this single `context.with`
                // closure, so they have the same `'js` lifetime — Persistent::save
                // compiles correctly here.
                let persistent = rquickjs::Persistent::save(&ctx, resolve);

                let mut st = self.state.lock().unwrap();
                if let Some(call) = st.staged.remove(&id) {
                    st.pending.insert(id, (call, persistent));
                }
            }

            // Clear the queue so it doesn't accumulate across rounds.
            ctx.eval::<(), _>(b"__auwgent_queue = [];")?;
            Ok(())
        })?;
        Ok(())
    }

    /// Pump the microtask queue, then drain the resolver queue, then decide:
    /// - No pending → `Finished`
    /// - Pending remain → `YieldedForTools`
    fn drive_to_yield(&mut self) -> JsResult<ExecutionResult> {
        // Run all microtasks that are currently queued.
        loop {
            match self.runtime.execute_pending_job() {
                Ok(true)  => continue,
                Ok(false) => break,
                Err(e)    => return Ok(ExecutionResult::Error(format!("Runtime error: {e}"))),
            }
        }

        // Drain the JS resolver queue into SharedState::pending.
        self.drain_resolvers()?;

        let st = self.state.lock().unwrap();

        if st.pending.is_empty() {
            Ok(ExecutionResult::Finished {
                ret_val: None,
                console_output: st.console.clone(),
                orphaned_calls: Vec::new(),
            })
        } else {
            let mut pairs: Vec<(u64, ToolCall)> = st
                .pending
                .iter()
                .map(|(id, (call, _))| (*id, call.clone()))
                .collect();
            pairs.sort_unstable_by_key(|(id, _)| *id);
            Ok(ExecutionResult::YieldedForTools {
                tools: pairs.into_iter().map(|(_, c)| c).collect(),
            })
        }
    }
}

impl Default for QuickJsSandbox {
    fn default() -> Self {
        Self::new().expect("Failed to create default QuickJsSandbox")
    }
}

impl Drop for QuickJsSandbox {
    /// Ensure all `Persistent<Function>` handles are dropped **before** the
    /// `Runtime` is destroyed. QuickJS asserts that its GC object list is empty
    /// at shutdown — if any `Persistent` values outlive the runtime we hold
    /// live JS object references and the assertion fires.
    fn drop(&mut self) {
        if let Ok(mut st) = self.state.lock() {
            // Drop all Persistent resolve handles — this decrements their
            // QuickJS reference count before the runtime shuts down.
            st.pending.clear();
            st.staged.clear();
        }
        // `runtime` and `context` are dropped in field order after this.
    }
}

// ─── Value Conversion Helpers ─────────────────────────────────────────────────

/// JS Value → display string (for console.log).
fn js_val_to_string(val: &Value) -> String {
    if val.is_null() || val.is_undefined() { return "null".to_string(); }
    if let Some(b) = val.as_bool()  { return b.to_string(); }
    if let Some(i) = val.as_int()   { return i.to_string(); }
    if let Some(f) = val.as_float() {
        return if f.fract() == 0.0 && f.abs() < 1e15 && f.is_finite() {
            format!("{}", f as i64)
        } else {
            f.to_string()
        };
    }
    if let Some(s) = val.as_string() { return s.to_string().unwrap_or_default(); }
    js_val_to_json(val).to_string()
}

/// JS `Value` → `serde_json::Value` (recursive).
pub fn js_val_to_json(val: &Value) -> serde_json::Value {
    if val.is_null() || val.is_undefined() { return serde_json::Value::Null; }
    if let Some(b) = val.as_bool()  { return serde_json::Value::Bool(b); }
    if let Some(i) = val.as_int()   { return serde_json::json!(i); }
    if let Some(f) = val.as_float() { return serde_json::json!(f); }
    if let Some(s) = val.as_string() {
        return serde_json::Value::String(s.to_string().unwrap_or_default());
    }
    if let Some(arr) = val.as_array() {
        let items = (0..arr.len())
            .filter_map(|i| arr.get::<Value>(i).ok().as_ref().map(js_val_to_json))
            .collect();
        return serde_json::Value::Array(items);
    }
    if let Some(obj) = val.as_object() {
        let mut map = serde_json::Map::new();
        // keys::<K>() returns a plain iterator, not a Result.
        for key in obj.keys::<rquickjs::String>().flatten() {
            let k = key.to_string().unwrap_or_default();
            // get<K: IntoAtom, V: FromJs>(k: K) → the first turbofish arg is K.
            if let Ok(v) = obj.get::<rquickjs::String, Value>(key) {
                map.insert(k, js_val_to_json(&v));
            }
        }
        return serde_json::Value::Object(map);
    }
    serde_json::Value::Null
}

/// `serde_json::Value` → `rquickjs::Value<'js>`.
pub fn json_to_js<'js>(
    ctx: &rquickjs::Ctx<'js>,
    val: &serde_json::Value,
) -> rquickjs::Result<Value<'js>> {
    match val {
        serde_json::Value::Null    => Ok(Value::new_null(ctx.clone())),
        serde_json::Value::Bool(b) => Ok(Value::new_bool(ctx.clone(), *b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Value::new_int(ctx.clone(), i as i32))
            } else {
                Ok(Value::new_float(ctx.clone(), n.as_f64().unwrap_or(0.0)))
            }
        }
        serde_json::Value::String(s) => {
            Ok(rquickjs::String::from_str(ctx.clone(), s)?.into_value())
        }
        serde_json::Value::Array(arr) => {
            let js_arr = rquickjs::Array::new(ctx.clone())?;
            for (i, item) in arr.iter().enumerate() {
                js_arr.set(i, json_to_js(ctx, item)?)?;
            }
            Ok(js_arr.into_value())
        }
        serde_json::Value::Object(map) => {
            let obj = Object::new(ctx.clone())?;
            for (k, v) in map {
                obj.set(k.as_str(), json_to_js(ctx, v)?)?;
            }
            Ok(obj.into_value())
        }
    }
}
