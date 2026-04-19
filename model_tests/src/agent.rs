/// Auwgent agent loop.
///
/// Handles the full lifecycle:
///   1. Build system prompt from writing rules + tool descriptions
///   2. Call Groq to get a Lua script via the `execute_lua_script` function call
///   3. Execute the script in a fresh `AuwgentSandbox`
///   4. Drive the yield loop — mock tool calls with the scenario dispatcher
///   5. Return a fully-populated `AgentRun` for validation
use std::time::Instant;

use auwgent_mode::{AuwgentSandbox, ExecutionResult, ToolCall, ToolDefinition, ToolResult};

use crate::client::{GroqClient, Message};

// ── Result types ──────────────────────────────────────────────────────────────

/// Everything the validator needs to assess a completed agent run.
#[derive(Debug)]
pub struct AgentRun {
    /// The raw Lua script the model produced.
    pub lua_script: String,
    /// Number of `YieldedForTools` rounds before `Finished`.
    pub tool_rounds: usize,
    /// Everything `print()`'d by the Lua script.
    pub console_output: String,
    /// The value the script explicitly `return`'d, if any.
    pub ret_val: Option<String>,
    /// Tool calls built but never passed to `await_all()`.
    pub orphaned_calls: Vec<ToolCall>,
    /// Wall-clock time from LLM call to `Finished`, in ms.
    pub duration_ms: u128,
    /// Set if either the API call or engine execution produced an error.
    pub error: Option<String>,
}

// ── Agent ─────────────────────────────────────────────────────────────────────

pub struct AuwgentAgent<'a> {
    pub client: &'a GroqClient,
    pub tools: Vec<ToolDefinition>,
    /// Optional globals to inject into the sandbox before execution.
    pub globals: Option<serde_json::Value>,
}

impl<'a> AuwgentAgent<'a> {
    /// Run a task end-to-end:
    /// - Get Lua from the model via function calling
    /// - Execute it in the sandbox, dispatching mocked tool results
    /// - Return the full `AgentRun` for validation
    pub fn run(
        &self,
        user_task: &str,
        dispatcher: &dyn Fn(&str, &serde_json::Value) -> serde_json::Value,
    ) -> AgentRun {
        let start = Instant::now();

        // Build the system prompt:
        //   - Writing rules that constrain model behaviour
        //   - Dynamic tool list from the scenario's registered tools
        let tool_prompt = AuwgentSandbox::generate_tool_prompt(&self.tools);
        let system = format!(
            "You are an AI agent using Auwgent Mode (a Lua sandbox).\n\
             When given a task, call `execute_lua_script` with a complete Lua script.\n\n\
             Rules:\n\
             - ALWAYS wrap every tool call in await_all(). Example:\n\
               `local r = await_all(get_weather({{ location = \"Lagos\" }}))`\n\
             - Never call a tool without await_all() — the call will be silently ignored.\n\
             - Use print() to return your result that will be available to you for your next action.\n\
             - Return a final value with `return` if the task demands a specific answer.\n\
             - Do NOT use os, io, require, or any external library.\n\n\
             Available tools (call inside your Lua script using await_all):\n\
             {tool_prompt}"
        );

        // The description passed as the function's `description` field also
        // carries the tool list so the model sees it in the function schema itself.
        let fn_description = format!(
            "Submit a Lua script to the Auwgent sandbox to complete the task.\n\
             Use await_all() for every tool call.\n\n\
             Available tools:\n{tool_prompt}"
        );

        let messages = vec![
            Message {
                role: "system".to_string(),
                content: Some(system),
                tool_calls: None,
                tool_call_id: None,
            },
            Message {
                role: "user".to_string(),
                content: Some(user_task.to_string()),
                tool_calls: None,
                tool_call_id: None,
            },
        ];

        // ── Step 1: get Lua from the model ────────────────────────────────────
        let (lua_script, _call_id) = match self.client.get_lua_script(&messages, &fn_description) {
            Ok(pair) => pair,
            Err(e) => {
                return AgentRun {
                    lua_script: String::new(),
                    tool_rounds: 0,
                    console_output: String::new(),
                    ret_val: None,
                    orphaned_calls: Vec::new(),
                    duration_ms: start.elapsed().as_millis(),
                    error: Some(e),
                };
            }
        };

        // ── Step 2: set up the sandbox ────────────────────────────────────────
        let mut engine = AuwgentSandbox::new().unwrap();
        engine.register_tools(&self.tools).unwrap();

        if let Some(globals) = &self.globals {
            engine.inject_globals(globals.clone()).unwrap();
        }

        // ── Step 3: drive the execution loop ──────────────────────────────────
        let mut status = engine.execute(&lua_script).unwrap();
        let mut tool_rounds = 0usize;

        loop {
            match status {
                ExecutionResult::YieldedForTools { ref tools } => {
                    tool_rounds += 1;

                    // Dispatch every yielded tool through the scenario's mock dispatcher.
                    let results: Vec<ToolResult> = tools
                        .iter()
                        .map(|t| ToolResult::Ok(dispatcher(&t.tool_name, &t.payload)))
                        .collect();

                    status = engine.resume_with_results(results).unwrap();
                }

                ExecutionResult::Finished {
                    ret_val,
                    console_output,
                    orphaned_calls,
                } => {
                    return AgentRun {
                        lua_script,
                        tool_rounds,
                        console_output,
                        ret_val,
                        orphaned_calls,
                        duration_ms: start.elapsed().as_millis(),
                        error: None,
                    };
                }

                ExecutionResult::Error(e) => {
                    return AgentRun {
                        lua_script,
                        tool_rounds,
                        console_output: engine.get_console_output(),
                        ret_val: None,
                        orphaned_calls: Vec::new(),
                        duration_ms: start.elapsed().as_millis(),
                        error: Some(e),
                    };
                }
            }
        }
    }
}
