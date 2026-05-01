use mlua::{Lua, LuaSerdeExt, MultiValue, RegistryKey, Result as LuaResult, StdLib, Thread};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::types::{
    ExecutionResult, SandboxSnapshot, ToolCall, ToolDefinition, ToolResult,
};

// ─── Print Utility ───────────────────────────────────────────────────────────

/// Recursively converts any Lua value to a human-readable string.
///
/// This is the backing implementation for the sandboxed `print` function.
/// Unlike `lua.coerce_string()` — which emits `table: 0x7f...` for tables —
/// this function walks the table and renders it as `{ key: value, ... }`.
///
/// Rules:
/// - Arrays (consecutive integer keys starting at 1) are rendered first, positionally.
/// - Hash keys are rendered as `key: value` pairs.
/// - Recursion is capped at depth 5 (`{...}` is emitted beyond that).
/// - nil → `"nil"`, booleans → `"true"`/`"false"`, numbers match Lua's `%g` format.
/// - Functions, userdata, and threads fall back to their Lua type name.
fn lua_val_to_string(lua: &Lua, val: mlua::Value, depth: u8) -> String {
    const MAX_DEPTH: u8 = 5;

    match val {
        mlua::Value::Nil            => "nil".to_string(),
        mlua::Value::Boolean(b)     => if b { "true" } else { "false" }.to_string(),
        mlua::Value::Integer(i)     => i.to_string(),
        mlua::Value::Number(n) => {
            // Replicate Lua's %g formatting: drop the decimal point for whole numbers.
            if n.fract() == 0.0 && n.abs() < 1e15 && n.is_finite() {
                format!("{}", n as i64)
            } else {
                format!("{n}")
            }
        }
        mlua::Value::String(s)      => s.to_string_lossy().to_string(),
        mlua::Value::Table(t) => {
            if depth >= MAX_DEPTH {
                return "{...}".to_string();
            }

            let mut parts: Vec<String> = Vec::new();

            // ── Array section ────────────────────────────────────────────────
            // Walk consecutive integer keys from 1 upward before hash keys so
            // positional results (e.g. tool return values) render naturally.
            let mut arr_len: i64 = 0;
            loop {
                match t.raw_get::<mlua::Value>(arr_len + 1) {
                    Ok(v) if !matches!(v, mlua::Value::Nil) => {
                        parts.push(lua_val_to_string(lua, v, depth + 1));
                        arr_len += 1;
                    }
                    _ => break,
                }
            }

            // ── Hash section ─────────────────────────────────────────────────
            // Emit all non-array key-value pairs as `key: value`.
            for pair in t.pairs::<mlua::Value, mlua::Value>() {
                if let Ok((k, v)) = pair {
                    // Skip integer indices already emitted above.
                    if let mlua::Value::Integer(i) = &k {
                        if *i >= 1 && *i <= arr_len {
                            continue;
                        }
                    }
                    let key = match &k {
                        mlua::Value::String(s) => s.to_string_lossy().to_string(),
                        other => format!("[{}]", lua_val_to_string(lua, other.clone(), depth + 1)),
                    };
                    parts.push(format!("{key}: {}", lua_val_to_string(lua, v, depth + 1)));
                }
            }

            if parts.is_empty() {
                "{}".to_string()
            } else {
                format!("{{ {} }}", parts.join(", "))
            }
        }
        // Functions, threads, userdata — no meaningful string representation.
        other => other.type_name().to_string(),
    }
}

// ─── LuauSandbox ─────────────────────────────────────────────────────────────

pub struct LuauSandbox {
    lua: Lua,
    active_thread: Option<RegistryKey>,
    instr_count: Arc<AtomicUsize>,
    /// Tracks whether sandbox(true) has been applied.
    /// We apply it lazily on the first execute() call, AFTER all tools and globals
    /// have been registered on the (still mutable) globals table.
    is_locked: bool,

    // ── Snapshot tracking ──────────────────────────────────────────────────────
    // These fields accumulate the session history automatically so that
    // snapshot() is free — callers do not need to track anything themselves.
    /// The script currently being executed (set by execute()).
    snapshot_script: Option<String>,
    /// All tool result rounds injected so far, in order.
    snapshot_rounds: Vec<Vec<serde_json::Value>>,
    /// A copy of all registered ToolDefinitions for snapshot reconstruction.
    snapshot_tools: Vec<ToolDefinition>,
    /// A copy of all injected globals for snapshot reconstruction.
    snapshot_globals: serde_json::Value,
    /// A copy of all library chunks loaded via load_library().
    snapshot_libraries: Vec<String>,

    // ── Orphan tracking ────────────────────────────────────────────────────────
    // All mutable tracking state is kept in Rust (Arc<Mutex<...>>) because
    // Luau's sandbox(true) recursively freezes tables reachable from the global
    // env — so we cannot use Lua-side tables for mutable tracking after the
    // first execute() call. The `__reg_intent` Rust closure registered before
    // lock is called by stubs at runtime; function calls are always sandbox-safe.
    /// Maps unique TID → ToolCall for every intent built by a tool stub.
    /// Entries are removed when properly yielded via await_all. Remaining
    /// entries at script completion are orphaned (never-yielded) calls.
    intent_registry: Arc<Mutex<HashMap<u64, ToolCall>>>,
}

impl LuauSandbox {
    /// Create a new restricted Luau sandbox.
    ///
    /// NOTE: `sandbox(true)` is NOT applied here. It is applied lazily on the
    /// first call to `execute()`, after the caller has had a chance to
    /// register tools and inject globals. This is the correct order because
    /// `sandbox(true)` freezes the global table to read-only.
    pub fn new() -> LuaResult<Self> {
        // Only load safe standard libraries — io, os, package, debug are omitted.
        let std_libs =
            StdLib::MATH | StdLib::STRING | StdLib::TABLE | StdLib::UTF8 | StdLib::COROUTINE;

        let lua = Lua::new_with(std_libs, mlua::LuaOptions::new().catch_rust_panics(true))?;

        // 1. Hard memory limit (20 MB) — prevents nested-table allocation exploits.
        lua.set_memory_limit(20 * 1024 * 1024)?;

        // 2. Hijack `print` to route output into a Rust-owned buffer.
        //    This gives the host full visibility of the LLM's execution trace.
        let print_buffer = Arc::new(Mutex::new(String::new()));
        lua.set_app_data(print_buffer.clone());

        let print_func = lua.create_function(|lua, args: mlua::MultiValue| {
            let arc = lua.app_data_ref::<Arc<Mutex<String>>>().unwrap();
            let mut buf = arc.lock().unwrap();

            // Build one tab-separated line, exactly as native Lua print does,
            // but with tables serialized to readable text instead of addresses.
            let mut line = String::new();
            for (i, val) in args.into_iter().enumerate() {
                if i > 0 {
                    line.push('\t');
                }
                line.push_str(&lua_val_to_string(lua, val, 0));
            }
            buf.push_str(&line);
            buf.push('\n');

            Ok(())
        })?;
        lua.globals().set("print", print_func)?;

        // 3. `await_all` coroutine bridge.
        //    A thin wrapper around coroutine.yield so the LLM can write
        //    synchronous-looking tool calls. Orphan tracking is handled on
        //    the Rust side via `__reg_intent`, not inside `await_all` itself.
        let wrapper_code = r#"
            function await_all(...)
                return coroutine.yield(...)
            end
        "#;
        lua.load(wrapper_code).exec()?;

        // 4. Orphan-tracking Rust infrastructure.
        //
        // Problem: Luau's sandbox(true) recursively freezes everything reachable
        // from the global env, including any table we store there. We cannot
        // mutate a Lua-side table from within a tool stub after sandbox lock.
        //
        // Solution: keep all mutable state in Rust (Arc<Mutex<...>>) and expose
        // a single Rust closure `__reg_intent` as a Lua global. Calling a
        // global function is always sandbox-safe — only global *writes* and table
        // *mutations* are restricted. The closure writes into the Rust HashMap
        // transparently.
        //
        // Protocol:
        //   1. Each stub calls `__reg_intent(name, args)` → gets a unique TID.
        //   2. The stub embeds `__tid = TID` in the intent table it returns.
        //   3. resume_internal reads `__tid` from each yielded table and removes
        //      the entry from intent_registry (the intent reached Rust safely).
        //   4. Whatever remains in intent_registry at Finished is an orphan.
        let intent_registry: Arc<Mutex<HashMap<u64, ToolCall>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let intent_counter: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));

        let reg_registry = intent_registry.clone();
        let reg_counter  = intent_counter.clone();
        let reg_fn = lua.create_function(
            move |lua_ctx, (name, payload_val): (String, mlua::Value)| {
                let mut ctr = reg_counter.lock().unwrap();
                *ctr += 1;
                let tid = *ctr;

                let payload: serde_json::Value = lua_ctx
                    .from_value(payload_val)
                    .unwrap_or(serde_json::json!({}));

                reg_registry
                    .lock()
                    .unwrap()
                    .insert(tid, ToolCall { tool_name: name, payload });

                // Return the TID to Lua so the stub can embed it in the intent table.
                Ok(tid as i64)
            },
        )?;
        lua.globals().set("__reg_intent", reg_fn)?;

        // 5. Instruction-count interrupt (Infinite Loop Protection).
        //    Luau's VM calls this hook periodically. We count those pings.
        //    This is timer-free and Yield-safe: the counter is reset on each
        //    new execute() call, not by wall-clock time.
        const MAX_INTERRUPTS: usize = 100_000;
        let instr_count = Arc::new(AtomicUsize::new(0));
        let hook_count = instr_count.clone();

        lua.set_interrupt(move |_| {
            let current = hook_count.fetch_add(1, Ordering::Relaxed);
            if current > MAX_INTERRUPTS {
                Err(mlua::Error::RuntimeError(
                    "Instruction limit exceeded (Infinite loop detected!)".into(),
                ))
            } else {
                Ok(mlua::VmState::Continue)
            }
        });

        Ok(Self {
            lua,
            active_thread: None,
            instr_count,
            is_locked: false,
            snapshot_script: None,
            snapshot_rounds: Vec::new(),
            snapshot_tools: Vec::new(),
            snapshot_globals: serde_json::Value::Object(serde_json::Map::new()),
            snapshot_libraries: Vec::new(),
            intent_registry,
        })
    }

    // ─── Configuration API ────────────────────────────────────────────────────

    /// Inject read-only context variables into the Lua global scope.
    /// Must be called BEFORE `execute()` — after the first execution,
    /// the sandbox is locked and globals become immutable.
    ///
    /// All injected globals are automatically tracked for snapshot restoration.
    pub fn inject_globals(&mut self, ctx: serde_json::Value) -> LuaResult<()> {
        // Merge into snapshot tracker before mutating the VM
        if let (serde_json::Value::Object(stored), serde_json::Value::Object(incoming)) =
            (&mut self.snapshot_globals, &ctx)
        {
            for (k, v) in incoming {
                stored.insert(k.clone(), v.clone());
            }
        }

        if let serde_json::Value::Object(map) = ctx {
            for (key, val) in map {
                let lua_val = self.lua.to_value(&val)?;
                self.lua.globals().set(key.as_str(), lua_val)?;
            }
        }
        Ok(())
    }

    /// Register a list of `ToolDefinition`s available to the LLM.
    /// Automatically generates idiomatic Lua function stubs for each tool.
    /// Must be called BEFORE `execute()`.
    ///
    /// Definitions are automatically tracked for snapshot restoration.
    pub fn register_tools(&mut self, tools: &[ToolDefinition]) -> LuaResult<()> {
        // Track for snapshot
        self.snapshot_tools.extend_from_slice(tools);

        let mut script = String::new();
        for t in tools {
            if t.has_args {
                // Stub calls the Rust closure `__reg_intent(name, args)` to register
                // the intent in the engine's HashMap and get back a unique TID.
                // The TID is embedded as `__tid` in the returned intent table.
                // resume_internal removes the TID when it sees the intent via yield.
                // Any TID remaining in the registry at Finished = orphan.
                script.push_str(&format!(
                    r#"
                    function {}(args)
                        local tid = __reg_intent("{}", args)
                        return {{ name = "{}", payload = args, __tid = tid }}
                    end
                    "#,
                    t.name, t.name, t.name
                ));
            } else {
                script.push_str(&format!(
                    r#"
                    function {}()
                        local tid = __reg_intent("{}", nil)
                        return {{ name = "{}", __tid = tid }}
                    end
                    "#,
                    t.name, t.name, t.name
                ));
            }
        }
        self.lua.load(&script).exec()?;
        Ok(())
    }

    /// Load reusable Lua library code (utilities, helper functions) that the LLM
    /// can call without needing to rewrite it in every script.
    ///
    /// Unlike `register_tools`, library functions are pure Lua — they run
    /// entirely inside the VM and do not yield to the Rust host.
    ///
    /// Must be called BEFORE `execute()`. Library code is tracked for snapshot
    /// restoration so helper functions are available after an engine is rebuilt.
    pub fn load_library(&mut self, lua_code: &str) -> LuaResult<()> {
        self.snapshot_libraries.push(lua_code.to_string());
        self.lua.load(lua_code).exec()?;
        Ok(())
    }

    /// Generates a system prompt block describing all registered tools.
    /// Feed this into your LLM system prompt so the model knows what tools exist.
    pub fn generate_tool_prompt(tools: &[ToolDefinition]) -> String {
        let mut out = String::from("You have access to the following tools:\n\n");
        for t in tools {
            out.push_str(&format!("- `{}`: {}", t.name, t.description));
            if t.has_args {
                if let Some(schema) = &t.arg_schema {
                    out.push_str(&format!(" Args: `{}`", schema));
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

    /// Read everything the LLM `print()`'d since the last `execute()` call.
    pub fn get_console_output(&self) -> String {
        if let Some(arc) = self.lua.app_data_ref::<Arc<Mutex<String>>>() {
            arc.lock().unwrap().clone()
        } else {
            String::new()
        }
    }

    /// Load and execute a Luau script in the sandbox.
    /// On the first call, this locks the sandbox by applying `sandbox(true)`.
    ///
    /// The script source is recorded internally for snapshot support.
    pub fn execute(&mut self, source: &str) -> LuaResult<ExecutionResult> {
        // Apply sandbox(true) exactly once — after registration, before execution.
        // This freezes the global table so the LLM script cannot redefine tools or print.
        if !self.is_locked {
            self.lua.sandbox(true)?;
            self.is_locked = true;
        }

        // Record the script source and reset the round history for this execution.
        self.snapshot_script = Some(source.to_string());
        self.snapshot_rounds.clear();

        // Clear the orphan registry so a re-used engine starts each script fresh.
        self.intent_registry.lock().unwrap().clear();

        // Reset the interrupt counter for this execution window.
        self.instr_count.store(0, Ordering::Relaxed);

        let func = self.lua.load(source).into_function()?;
        let thread = self.lua.create_thread(func)?;
        self.active_thread = Some(self.lua.create_registry_value(thread)?);

        // Clear print buffer so each execute() starts with a fresh trace.
        if let Some(arc) = self.lua.app_data_ref::<Arc<Mutex<String>>>() {
            arc.lock().unwrap().clear();
        }

        self.resume_internal(MultiValue::new())
    }

    /// Resume a suspended script, injecting tool result JSON payloads back
    /// into the Lua coroutine stack as native Lua values.
    ///
    /// The order of `next_values` must match the order of tools that were yielded.
    /// Results are automatically recorded for snapshot support.
    pub fn resume_with_json(
        &mut self,
        next_values: Vec<serde_json::Value>,
    ) -> LuaResult<ExecutionResult> {
        // Record this round before injecting
        self.snapshot_rounds.push(next_values.clone());

        let lua_vals: Vec<mlua::Value> = next_values
            .into_iter()
            .map(|v| self.lua.to_value(&v).unwrap_or(mlua::Value::Nil))
            .collect();

        self.resume_internal(MultiValue::from_vec(lua_vals))
    }

    /// Resume a suspended script with structured per-tool results that can include failures.
    ///
    /// Unlike `resume_with_json`, this method accepts a mix of `ToolResult::Ok` and
    /// `ToolResult::Err`. Error results are injected as a sentinel table:
    /// `{ __error = true, message = "..." }` so the LLM can handle each failure
    /// independently without crashing the entire script.
    ///
    /// The order of `results` must match the order of tools in the last `YieldedForTools`.
    /// Results are materialized to `serde_json::Value` and recorded for snapshot support,
    /// ensuring `ToolResult::Err` sentinel tables are faithfully reproduced on restore.
    pub fn resume_with_results(&mut self, results: Vec<ToolResult>) -> LuaResult<ExecutionResult> {
        // Materialize each ToolResult to a plain JSON value for storage and injection.
        // ToolResult::Err becomes { "__error": true, "message": "..." } which,
        // when re-injected via resume_with_json on restore, produces the same Lua table.
        let json_vals: Vec<serde_json::Value> = results
            .into_iter()
            .map(|r| match r {
                ToolResult::Ok(v) => v,
                ToolResult::Err(msg) => serde_json::json!({ "__error": true, "message": msg }),
            })
            .collect();

        // Delegate to resume_with_json which handles tracking + injection
        self.resume_with_json(json_vals)
    }

    // ─── Snapshot API ─────────────────────────────────────────────────────────

    /// Capture the current execution state as a serializable snapshot.
    ///
    /// The snapshot can be stored to a database, file, or any persistent medium.
    /// Call `LuauSandbox::from_snapshot()` to restore the engine to this exact
    /// point, fast-forwarding through all previously-resolved tool yields automatically.
    ///
    /// Returns `None` if `execute()` has not been called yet.
    pub fn snapshot(&self) -> Option<SandboxSnapshot> {
        Some(SandboxSnapshot {
            script_source: self.snapshot_script.clone()?,
            completed_tool_results: self.snapshot_rounds.clone(),
            tool_definitions: self.snapshot_tools.clone(),
            injected_globals: self.snapshot_globals.clone(),
            libraries: self.snapshot_libraries.clone(),
        })
    }

    /// Restore a sandbox from a snapshot, fast-forwarding through all previously
    /// completed tool yield rounds to reach the next un-resolved yield point.
    ///
    /// # Restoration process
    /// 1. A fresh `LuauSandbox` is created.
    /// 2. Libraries, tools, and globals from the snapshot are reloaded.
    /// 3. The original script is executed from the beginning.
    /// 4. Every yield that has a cached result is fast-forwarded immediately
    ///    by injecting the stored JSON — no real tools are called.
    /// 5. The first yield with no cached result is returned to the caller
    ///    as `ExecutionResult::YieldedForTools`, ready for real execution.
    ///
    /// Returns the restored engine and the current `ExecutionResult` — which
    /// will be the next un-resolved yield, or `Finished` if the script ran
    /// to completion from the cached history alone.
    pub fn from_snapshot(snapshot: SandboxSnapshot) -> LuaResult<(Self, ExecutionResult)> {
        let mut engine = LuauSandbox::new()?;

        // Restore library functions first (before tools, following registration order)
        for lib in &snapshot.libraries {
            engine.load_library(lib)?;
        }

        // Restore tool stubs
        engine.register_tools(&snapshot.tool_definitions)?;

        // Restore injected global variables
        engine.inject_globals(snapshot.injected_globals.clone())?;

        // Start executing the original script from the beginning
        let mut status = engine.execute(&snapshot.script_source)?;

        // Fast-forward through every yield round that already has a cached result.
        // We do NOT call real tools for these — we replay the stored JSON directly.
        for cached_round in snapshot.completed_tool_results {
            match status {
                ExecutionResult::YieldedForTools { .. } => {
                    status = engine.resume_with_json(cached_round)?;
                }
                // Script finished or errored before all caches were consumed —
                // the snapshot may be stale or the script non-deterministic.
                ExecutionResult::Finished { .. } | ExecutionResult::Error(_) => break,
            }
        }

        Ok((engine, status))
    }

    // ─── Internal ─────────────────────────────────────────────────────────────

    /// Read any tool intents that were built by stubs but never passed to `await_all()`.
    ///
    /// Returns whatever remains in `intent_registry` — entries are removed as
    /// intents are properly yielded through the Resumable branch of resume_internal.
    /// Whatever is left when the script finishes was built but silently discarded.
    fn collect_orphans(&self) -> Vec<ToolCall> {
        self.intent_registry.lock().unwrap().values().cloned().collect()
    }

    fn resume_internal(&mut self, args: MultiValue) -> LuaResult<ExecutionResult> {
        let thread_key = match &self.active_thread {
            Some(key) => key,
            None => return Ok(ExecutionResult::Error("No active thread".into())),
        };

        let thread: Thread = self.lua.registry_value(thread_key)?;

        let result: MultiValue = thread
            .resume(args)
            .map_err(|e| mlua::Error::RuntimeError(format!("Execution failed: {}", e)))?;

        match thread.status() {
            mlua::ThreadStatus::Resumable => {
                // The thread yielded — parse the yielded tables as tool intents.
                let mut tools = Vec::new();
                for val in result.into_iter() {
                    if let mlua::Value::Table(t) = val {
                        let tool_name: Option<String> = t.get("name").ok();
                        let payload_val: mlua::Value = t.get("payload").unwrap_or(mlua::Value::Nil);

                        // This intent reached Rust via await_all — deregister it so it
                        // is never reported as an orphan. The TID was embedded in the
                        // intent table by the stub when it called __reg_intent.
                        let tid: Option<i64> = t.get("__tid").ok();
                        if let Some(id) = tid {
                            self.intent_registry.lock().unwrap().remove(&(id as u64));
                        }

                        if let Some(name) = tool_name {
                            // Deserialize the Lua table payload into a serde_json::Value.
                            // The LLM never has to manually call JSON.stringify — we do it here.
                            let payload: serde_json::Value = self
                                .lua
                                .from_value(payload_val)
                                .unwrap_or(serde_json::json!({}));

                            tools.push(ToolCall {
                                tool_name: name,
                                payload,
                            });
                        }
                    }
                }
                Ok(ExecutionResult::YieldedForTools { tools })
            }
            _ => {
                // The thread finished — collect any return values as strings.
                let mut ret_strings = Vec::new();
                for val in result.into_iter() {
                    if let Ok(Some(s)) = self.lua.coerce_string(val.clone()) {
                        ret_strings.push(s.to_string_lossy().to_string());
                    }
                }
                let ret_val = if ret_strings.is_empty() {
                    None
                } else {
                    Some(ret_strings.join(", "))
                };

                // Collect tool calls the LLM built without wrapping in await_all().
                // collect_orphans() is pure Rust — reads directly from intent_registry.
                let orphaned_calls = self.collect_orphans();

                Ok(ExecutionResult::Finished {
                    ret_val,
                    console_output: self.get_console_output(),
                    orphaned_calls,
                })
            }
        }
    }
}

impl Default for LuauSandbox {
    fn default() -> Self {
        Self::new().expect("Failed to create default LuauSandbox")
    }
}
