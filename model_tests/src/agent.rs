//! Auwgent live model test agent loop.

use std::time::Instant;

use auwgent_mode::{
    AuwgentSandbox, ExecutionResult, QuickJsSandbox, ToolCall, ToolDefinition, ToolResult,
};

use crate::client::{GroqClient, Message};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptLanguage {
    Lua,
    JavaScript,
}

impl ScriptLanguage {
    pub fn label(self) -> &'static str {
        match self {
            Self::Lua => "Lua",
            Self::JavaScript => "JavaScript",
        }
    }

    fn function_name(self) -> &'static str {
        match self {
            Self::Lua => "execute_lua_script",
            Self::JavaScript => "execute_js_script",
        }
    }

    fn tool_prompt(self, tools: &[ToolDefinition]) -> String {
        match self {
            Self::Lua => AuwgentSandbox::generate_tool_prompt(tools),
            Self::JavaScript => QuickJsSandbox::generate_tool_prompt(tools),
        }
    }

    fn system_prompt(self, tool_prompt: &str) -> String {
        match self {
            Self::Lua => format!(
                "You are an AI agent using Auwgent Mode (a Lua sandbox).\n\
                 When given a task, call `execute_lua_script` with a complete Lua script.\n\n\
                 Rules:\n\
                 - ALWAYS wrap every tool call in await_all(). Example:\n\
                   `local r = await_all(get_weather({{ location = \"Lagos\" }}))`\n\
                 - Never call a tool without await_all(); the call will be ignored.\n\
                 - Use print() to expose results.\n\
                 - Return a final value with `return` if the task demands a specific answer.\n\
                 - Do NOT use os, io, require, or any external library.\n\n\
                 Available tools (call inside your Lua script using await_all):\n\
                 {tool_prompt}"
            ),
            Self::JavaScript => format!(
                "You are an AI agent using Auwgent Mode (a QuickJS JavaScript sandbox).\n\
                 When given a task, call `execute_js_script` with a complete JavaScript script.\n\n\
                 Rules:\n\
                 - Tool functions are async. ALWAYS await every tool call. Example:\n\
                   `const r = await get_weather({{ location: \"Lagos\" }});`\n\
                 - For independent tools, batch them with Promise.all(). Example:\n\
                   `const [a, b] = await Promise.all([tool_a(), tool_b()]);`\n\
                 - Use console.log() to expose final results.\n\
                 - Do NOT use require, import, fetch, process, or host APIs.\n\n\
                 Available tools (call inside your JavaScript script with await):\n\
                 {tool_prompt}"
            ),
        }
    }

    fn function_description(self, tool_prompt: &str) -> String {
        match self {
            Self::Lua => format!(
                "Submit a Lua script to the Auwgent sandbox to complete the task.\n\
                 Use await_all() for every tool call.\n\n\
                 Available tools:\n{tool_prompt}"
            ),
            Self::JavaScript => format!(
                "Submit a JavaScript script to the QuickJS Auwgent sandbox to complete the task.\n\
                 Use await for every tool call and Promise.all() for independent parallel tools.\n\n\
                 Available tools:\n{tool_prompt}"
            ),
        }
    }

    fn body_description(self) -> &'static str {
        match self {
            Self::Lua => {
                "Complete valid Lua script. Use await_all() for every tool call. Do not include explanation outside the script."
            }
            Self::JavaScript => {
                "Complete valid JavaScript script for QuickJS. Use await for every tool call and console.log() for final output. Do not include explanation outside the script."
            }
        }
    }
}

#[derive(Debug)]
pub struct AgentRun {
    pub script: String,
    pub tool_rounds: usize,
    pub console_output: String,
    pub ret_val: Option<String>,
    pub orphaned_calls: Vec<ToolCall>,
    pub duration_ms: u128,
    pub error: Option<String>,
}

pub struct AuwgentAgent<'a> {
    pub client: &'a GroqClient,
    pub tools: Vec<ToolDefinition>,
    pub globals: Option<serde_json::Value>,
    pub language: ScriptLanguage,
}

impl<'a> AuwgentAgent<'a> {
    pub fn run(
        &self,
        user_task: &str,
        dispatcher: &dyn Fn(&str, &serde_json::Value) -> serde_json::Value,
    ) -> AgentRun {
        let start = Instant::now();
        let tool_prompt = self.language.tool_prompt(&self.tools);
        let system = self.language.system_prompt(&tool_prompt);
        let fn_description = self.language.function_description(&tool_prompt);

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

        let (script, _call_id) = match self.client.get_script(
            &messages,
            self.language.function_name(),
            &fn_description,
            self.language.body_description(),
        ) {
            Ok(pair) => pair,
            Err(e) => {
                return AgentRun {
                    script: String::new(),
                    tool_rounds: 0,
                    console_output: String::new(),
                    ret_val: None,
                    orphaned_calls: Vec::new(),
                    duration_ms: start.elapsed().as_millis(),
                    error: Some(e),
                };
            }
        };

        match self.language {
            ScriptLanguage::Lua => self.run_lua(script, start, dispatcher),
            ScriptLanguage::JavaScript => self.run_js(script, start, dispatcher),
        }
    }

    fn run_lua(
        &self,
        script: String,
        start: Instant,
        dispatcher: &dyn Fn(&str, &serde_json::Value) -> serde_json::Value,
    ) -> AgentRun {
        let mut engine = AuwgentSandbox::new().unwrap();
        engine.register_tools(&self.tools).unwrap();
        if let Some(globals) = &self.globals {
            engine.inject_globals(globals.clone()).unwrap();
        }

        let mut status = match engine.execute(&script) {
            Ok(status) => status,
            Err(e) => {
                return AgentRun {
                    script,
                    tool_rounds: 0,
                    console_output: engine.get_console_output(),
                    ret_val: None,
                    orphaned_calls: Vec::new(),
                    duration_ms: start.elapsed().as_millis(),
                    error: Some(e.to_string()),
                };
            }
        };

        let mut tool_rounds = 0usize;

        loop {
            match status {
                ExecutionResult::YieldedForTools { ref tools } => {
                    tool_rounds += 1;
                    let results: Vec<ToolResult> = tools
                        .iter()
                        .map(|t| ToolResult::Ok(dispatcher(&t.tool_name, &t.payload)))
                        .collect();

                    match engine.resume_with_results(results) {
                        Ok(next) => status = next,
                        Err(e) => {
                            return AgentRun {
                                script,
                                tool_rounds,
                                console_output: engine.get_console_output(),
                                ret_val: None,
                                orphaned_calls: Vec::new(),
                                duration_ms: start.elapsed().as_millis(),
                                error: Some(e.to_string()),
                            };
                        }
                    }
                }

                ExecutionResult::Finished {
                    ret_val,
                    console_output,
                    orphaned_calls,
                } => {
                    return AgentRun {
                        script,
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
                        script,
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

    fn run_js(
        &self,
        script: String,
        start: Instant,
        dispatcher: &dyn Fn(&str, &serde_json::Value) -> serde_json::Value,
    ) -> AgentRun {
        let mut engine = QuickJsSandbox::new().unwrap();
        engine.register_tools(&self.tools).unwrap();
        if let Some(globals) = &self.globals {
            engine.inject_globals(globals.clone()).unwrap();
        }

        let mut status = match engine.execute(&script) {
            Ok(status) => status,
            Err(e) => {
                return AgentRun {
                    script,
                    tool_rounds: 0,
                    console_output: engine.get_console_output(),
                    ret_val: None,
                    orphaned_calls: Vec::new(),
                    duration_ms: start.elapsed().as_millis(),
                    error: Some(e.to_string()),
                };
            }
        };
        let mut tool_rounds = 0usize;

        loop {
            match status {
                ExecutionResult::YieldedForTools { ref tools } => {
                    tool_rounds += 1;
                    let results: Vec<ToolResult> = tools
                        .iter()
                        .map(|t| ToolResult::Ok(dispatcher(&t.tool_name, &t.payload)))
                        .collect();

                    match engine.resume_with_results(results) {
                        Ok(next) => status = next,
                        Err(e) => {
                            return AgentRun {
                                script,
                                tool_rounds,
                                console_output: engine.get_console_output(),
                                ret_val: None,
                                orphaned_calls: Vec::new(),
                                duration_ms: start.elapsed().as_millis(),
                                error: Some(e.to_string()),
                            };
                        }
                    }
                }

                ExecutionResult::Finished {
                    ret_val,
                    console_output,
                    orphaned_calls,
                } => {
                    return AgentRun {
                        script,
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
                        script,
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
