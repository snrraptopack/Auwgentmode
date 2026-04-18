use mlua::{Lua, LuaSerdeExt, MultiValue, RegistryKey, Result as LuaResult, StdLib, Thread};
use std::sync::{Arc, Mutex};
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub tool_name: String,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub has_args: bool,
}

pub enum ExecutionResult {
    Finished {
        ret_val: Option<String>,
        console_output: String,
    },
    YieldedForTools {
        tools: Vec<ToolCall>,
    },
    Error(String),
}

pub struct AuwgentSandbox {
    lua: Lua,
    active_thread: Option<RegistryKey>,
}

impl AuwgentSandbox {
    /// Create a new restricted sandbox
    pub fn new() -> LuaResult<Self> {
        // Only load safe standard libraries
        // Omit io, os, package, debug
        let std_libs =
            StdLib::MATH | StdLib::STRING | StdLib::TABLE | StdLib::UTF8 | StdLib::COROUTINE;
        let lua = Lua::new_with(std_libs, mlua::LuaOptions::new().catch_rust_panics(true))?;

        // 1. Enter Luau Sandbox mode
        lua.sandbox(true)?;

        // 2. Set memory limit (e.g. 20 MB)
        lua.set_memory_limit(20 * 1024 * 1024)?;

        // 3. Inject Print Buffer
        let print_buffer = Arc::new(Mutex::new(String::new()));
        lua.set_app_data(print_buffer.clone());

        let print_func = lua.create_function(|lua, args: mlua::MultiValue| {
            let buffer_arc = lua.app_data_ref::<Arc<Mutex<String>>>().unwrap();
            let mut buffer = buffer_arc.lock().unwrap();

            let mut out = String::new();
            for (i, val) in args.into_iter().enumerate() {
                if i > 0 {
                    out.push('\t');
                }
                // Attempt to cast to boolean, string, or get type name
                if let mlua::Value::Boolean(b) = val {
                    out.push_str(if b { "true" } else { "false" });
                } else if let Ok(Some(s)) = lua.coerce_string(val.clone()) {
                    out.push_str(&s.to_string_lossy());
                } else {
                    out.push_str(val.type_name());
                }
            }
            buffer.push_str(&out);
            buffer.push('\n');

            Ok(())
        })?;
        lua.globals().set("print", print_func)?;

        // 4. Inject await_all mechanism
        // We inject a pure Lua function globally to proxy the tool payloads into a native yield.
        let wrapper_code = r#"
            function await_all(...)
                return coroutine.yield(...)
            end
        "#;
        lua.load(wrapper_code).exec()?;

        Ok(Self {
            lua,
            active_thread: None,
        })
    }

    /// Register a list of structured ToolDefinitions available to the LLM.
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

    /// Read the current print buffer
    pub fn get_console_output(&self) -> String {
        if let Some(arc) = self.lua.app_data_ref::<Arc<Mutex<String>>>() {
            let buf = arc.lock().unwrap();
            buf.clone()
        } else {
            String::new()
        }
    }

    pub fn execute(&mut self, source: &str) -> LuaResult<ExecutionResult> {
        let chunk = self.lua.load(source);
        let func = chunk.into_function()?;

        let thread = self.lua.create_thread(func)?;
        self.active_thread = Some(self.lua.create_registry_value(thread)?);

        // Clear the print buffer on new execution
        if let Some(arc) = self.lua.app_data_ref::<Arc<Mutex<String>>>() {
            let mut buf = arc.lock().unwrap();
            buf.clear();
        }

        self.resume_internal(MultiValue::new())
    }

    pub fn resume_with_json(&mut self, next_values: Vec<serde_json::Value>) -> LuaResult<ExecutionResult> {
        let lua_vals: Vec<mlua::Value> = next_values
            .into_iter()
            .map(|v| self.lua.to_value(&v).unwrap_or(mlua::Value::Nil))
            .collect();

        self.resume_internal(MultiValue::from_vec(lua_vals))
    }

    fn resume_internal(&mut self, args: MultiValue) -> LuaResult<ExecutionResult> {
        let thread_key = match &self.active_thread {
            Some(key) => key,
            None => return Ok(ExecutionResult::Error("No active thread".into())),
        };

        let thread: Thread = self.lua.registry_value(thread_key)?;

        // `resume` executes the thread until it yields or returns
        // We use `.resume::<_, MultiValue>(args)`
        let result: MultiValue = thread
            .resume(args)
            .map_err(|e| mlua::Error::RuntimeError(format!("Execution failed: {}", e)))?;

        match thread.status() {
            mlua::ThreadStatus::Resumable => {
                let mut tools = Vec::new();
                for val in result.into_iter() {
                    if let mlua::Value::Table(t) = val {
                        let tool_name: Option<String> = t.get("name").ok();
                        let payload_val: mlua::Value = t.get("payload").unwrap_or(mlua::Value::Nil);

                        // We safely convert the Lua value map into a native JSON Object on the Rust side
                        // This prevents the AI from needing to serialize JSON manually inside Lua!
                        if let Some(name) = tool_name {
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
