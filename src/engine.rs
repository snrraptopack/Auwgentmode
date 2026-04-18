use mlua::{Lua, LuaSerdeExt, MultiValue, RegistryKey, Result as LuaResult, StdLib, Thread};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// ─── Data Types ──────────────────────────────────────────────────────────────

/// A tool call intent yielded by the LLM script, ready for Rust to execute.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub tool_name: String,
    pub payload: serde_json::Value,
}

/// Describes a tool the LLM is allowed to call.
/// Use this to register tools AND auto-generate system prompt descriptions.
/// Derives Serialize/Deserialize so it can be stored in a SandboxSnapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    /// Human-readable description of what this tool does (used in prompt generation).
    pub description: String,
    /// Whether this tool accepts a named-parameter table as its single argument.
    pub has_args: bool,
    /// Optional schema hint shown to the LLM, e.g. `{ location: string, units: string }`.
    pub arg_schema: Option<String>,
}

/// The result of a single sandbox execution step.
#[derive(Debug)]
pub enum ExecutionResult {
    /// The script completed normally.
    Finished {
        ret_val: Option<String>,
        console_output: String,
    },
    /// The script paused and is waiting for these host tools to execute.
    YieldedForTools { tools: Vec<ToolCall> },
    /// A non-recoverable runtime error occurred inside the script.
    Error(String),
}

/// The outcome of a single host-side tool execution.
/// Use this with `resume_with_results()` to give the LLM structured
/// success or failure feedback without crashing the entire script.
#[derive(Debug)]
pub enum ToolResult {
    /// The tool succeeded. The value is injected as a Lua table into the script.
    Ok(serde_json::Value),
    /// The tool failed. Injected as `{ __error = true, message = "..." }` so the
    /// LLM can check `if result.__error then` and handle gracefully.
    Err(String),
}

/// A serializable point-in-time snapshot of a sandbox execution session.
///
/// Stores everything needed to reconstruct the engine and fast-forward
/// it to exactly the yield point it was at when the snapshot was taken.
/// This allows resuming execution across process restarts, HTTP request
/// boundaries, or any other stateless host environment.
///
/// # How it works
///
/// Rather than serializing the raw Lua VM state (which mlua does not
/// expose), the snapshot records the complete *history* of the session:
/// the original script, all completed tool result rounds, and the
/// registration configuration. On restore, a new VM replays the script
/// but automatically fast-forwards through every previously-resolved
/// yield — injecting cached results instead of calling real tools —
/// until it reaches the first *new* yield that needs real execution.
///
/// `ToolResult::Err` results are materialized as
/// `{ "__error": true, "message": "..." }` JSON before storage so that
/// replay via `resume_with_json` produces an identical Lua value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxSnapshot {
    /// The original Lua script source exactly as passed to `execute()`.
    pub script_source: String,
    /// All tool result rounds that have already been resolved and injected,
    /// in order. Each inner Vec corresponds to one `resume_with_json` or
    /// `resume_with_results` call.
    pub completed_tool_results: Vec<Vec<serde_json::Value>>,
    /// The tool definitions registered before execution, needed to rebuild
    /// the Lua function stubs in the restored engine.
    pub tool_definitions: Vec<ToolDefinition>,
    /// The globals injected before execution via `inject_globals()`.
    pub injected_globals: serde_json::Value,
    /// Any Lua library code loaded via `load_library()` before execution.
    pub libraries: Vec<String>,
}

// ─── Sandbox ─────────────────────────────────────────────────────────────────

pub struct AuwgentSandbox {
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
}

impl AuwgentSandbox {
    /// Create a new restricted sandbox.
    ///
    /// NOTE: `sandbox(true)` is NOT applied here. It is applied lazily on the
    /// first call to `execute()`, after the caller has had a chance to
    /// register tools and inject globals. This is the correct order because
    /// `sandbox(true)` freezes the global table to read-only.
    pub fn new() -> LuaResult<Self> {
        // Only load safe standard libraries — io, os, package, debug are omitted.
        let std_libs = StdLib::MATH
            | StdLib::STRING
            | StdLib::TABLE
            | StdLib::UTF8
            | StdLib::COROUTINE;

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

            let mut line = String::new();
            for (i, val) in args.into_iter().enumerate() {
                if i > 0 {
                    line.push('\t');
                }
                // Lua's native tostring() rules: booleans need special handling
                // because coerce_string does not convert them natively.
                if let mlua::Value::Boolean(b) = val {
                    line.push_str(if b { "true" } else { "false" });
                } else if let Ok(Some(s)) = lua.coerce_string(val.clone()) {
                    line.push_str(&s.to_string_lossy());
                } else {
                    line.push_str(val.type_name());
                }
            }
            buf.push_str(&line);
            buf.push('\n');

            Ok(())
        })?;
        lua.globals().set("print", print_func)?;

        // 3. Inject `await_all` — the coroutine bridge that lets the LLM
        //    write fully synchronous-looking tool calls while yielding to Rust.
        let wrapper_code = r#"
            function await_all(...)
                return coroutine.yield(...)
            end
        "#;
        lua.load(wrapper_code).exec()?;

        // 4. Instruction-count interrupt (Infinite Loop Protection).
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
        })
    }

    // ─── Configuration API ────────────────────────────────────────────────────

    /// Inject read-only context variables into the Lua global scope.
    /// Must be called BEFORE `execute()` — after the first execution,
    /// the sandbox is locked and globals become immutable.
    ///
    /// All injected globals are automatically tracked for snapshot restoration.
    ///
    /// Example:
    /// ```rust,no_run
    /// # use auwgent_mode::AuwgentSandbox;
    /// let mut engine = AuwgentSandbox::new().unwrap();
    /// engine.inject_globals(serde_json::json!({
    ///     "AGENT_ID": "agent_007",
    ///     "WORKSPACE_PATH": "/app/project"
    /// })).unwrap();
    /// ```
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
                script.push_str(&format!(
                    r#"
                    function {}(args)
                        return {{ name = "{}", payload = args }}
                    end
                    "#,
                    t.name, t.name
                ));
            } else {
                script.push_str(&format!(
                    r#"
                    function {}()
                        return {{ name = "{}" }}
                    end
                    "#,
                    t.name, t.name
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

    /// Load and execute a Lua script in the sandbox.
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
    pub fn resume_with_results(
        &mut self,
        results: Vec<ToolResult>,
    ) -> LuaResult<ExecutionResult> {
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
    /// Call `AuwgentSandbox::from_snapshot()` to restore the engine to this exact
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
    /// 1. A fresh `AuwgentSandbox` is created.
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
        let mut engine = AuwgentSandbox::new()?;

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
                        let payload_val: mlua::Value =
                            t.get("payload").unwrap_or(mlua::Value::Nil);

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

                Ok(ExecutionResult::Finished {
                    ret_val,
                    console_output: self.get_console_output(),
                })
            }
        }
    }
}

impl Default for AuwgentSandbox {
    fn default() -> Self {
        Self::new().expect("Failed to create default AuwgentSandbox")
    }
}
