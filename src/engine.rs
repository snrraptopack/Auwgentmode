use mlua::{Lua, LuaSerdeExt, MultiValue, RegistryKey, Result as LuaResult, StdLib, Thread};
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
#[derive(Debug, Clone)]
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

// ─── Sandbox ─────────────────────────────────────────────────────────────────

pub struct AuwgentSandbox {
    lua: Lua,
    active_thread: Option<RegistryKey>,
    instr_count: Arc<AtomicUsize>,
    /// Tracks whether sandbox(true) has been applied.
    /// We apply it lazily on the first execute() call, AFTER all tools and globals
    /// have been registered on the (still mutable) globals table.
    is_locked: bool,
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
        })
    }

    // ─── Configuration API ────────────────────────────────────────────────────

    /// Inject read-only context variables into the Lua global scope.
    /// Must be called BEFORE `execute()` — after the first execution,
    /// the sandbox is locked and globals become immutable.
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
    pub fn register_tools(&mut self, tools: &[ToolDefinition]) -> LuaResult<()> {
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
    pub fn execute(&mut self, source: &str) -> LuaResult<ExecutionResult> {
        // Apply sandbox(true) exactly once — after registration, before execution.
        // This freezes the global table so the LLM script cannot redefine tools or print.
        if !self.is_locked {
            self.lua.sandbox(true)?;
            self.is_locked = true;
        }

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
    pub fn resume_with_json(
        &mut self,
        next_values: Vec<serde_json::Value>,
    ) -> LuaResult<ExecutionResult> {
        let lua_vals: Vec<mlua::Value> = next_values
            .into_iter()
            .map(|v| self.lua.to_value(&v).unwrap_or(mlua::Value::Nil))
            .collect();

        self.resume_internal(MultiValue::from_vec(lua_vals))
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
