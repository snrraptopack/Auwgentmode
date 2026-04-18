# Auwgent Mode: The Monty-Inspired Lua Sandbox

Auwgent Mode is a secure, blazing-fast, in-process execution environment written in Rust, utilizing the heavily isolated **Luau** engine. 

It is designed to solve the "Code-Mode" Agentic LLM problem. Instead of forcing your Large Language Model (LLM) to perform slow, single-turn JSON tool calls over HTTP, you can prompt the LLM to write a single block of Lua logic. Auwgent Mode securely evaluates this untrusted code synchronously within your Rust application in micro-seconds, bridging natively to your host environment.

Inspired by Pydantic's [Monty](https://github.com/pydantic/monty), Auwgent Mode focuses on **deny-by-default runtime security**. 

## Features
- **Deny-by-Default Security:** Utilizes `Luau::sandbox(true)` via `mlua`. File Systems, Networking (`os`), and external file loading (`require`, `io`) are completely physically stripped from the VM.
- **Resource Walls:** Hardcoded 20MB execution heap limits to prevent malicious nested-table exploits.
- **Console Hijacking:** Secure real-time capturing of the `print()` function for Agent visibility.
- **Dynamic Tool Registration:** Register your tools passing a structured `ToolDefinition` list, and the Engine automatically builds elegant, abstracted Lua function stubs.
- **Named-Parameter DX:** Native conversion from AI-passed single-table maps into strict Rust JSON schemas, preventing hallucination while enforcing predictable payloads.
- **Synchronous LLM Scripting via Coroutine Yielding:** The AI script calls `await_all(tool_a(), tool_b())` which securely freezes the Sandbox. Rust manages all I/O via `tokio` out-of-band and smoothly `resume()`s the Sandbox context data.

## Getting Started: Integrations

If you are importing `auwgent_mode` into your backend agent framework, you will use the `AuwgentSandbox` API block to evaluate LLM scripts.

### Basic Architecture Flow
1. Instantiate the Engine.
2. Register your APIs via `ToolDefinition`s, telling the Engine whether to expect arguments.
3. Execute the model's parsed Lua string block natively.
4. If it hits a `Yield` for tools, intercept it.
5. Perform your Host actions concurrently (`reqwest`, database, etc).
6. Resume the Engine with exactly the data needed.

### Practical Developer Experience (DX) Example
```rust
use auwgent_mode::{AuwgentSandbox, ExecutionResult, ToolDefinition};

fn main() {
    // 1. Initialize our Sandboxed Instance
    let mut engine = AuwgentSandbox::new().expect("Failed to boot engine");

    // 2. Register tools so the LLM has elegant functions injected automatically
    engine.register_tools(&[
        ToolDefinition { name: "get_weather".to_string(), has_args: true },
        ToolDefinition { name: "get_time".to_string(), has_args: false }
    ]).unwrap();

    // The untrusted code exactly as written by the AI Agent.
    let ai_script = r#"
        print("Agent Task Initiated.")
        
        -- First Yield: The AI fetches the current time
        local current_time = await_all(get_time())[1]
        
        -- The AI can do string manipulations natively in the sandbox without breaking loops!
        local query_time = "Time is: " .. current_time.hour
        print(query_time)
        
        -- Second Yield: It runs another tool using its internal context State
        local ny_weather = await_all(get_weather({ location = "NY", units = "imperial" }))[1]
        
        print("Final weather condition:", ny_weather.condition)
        
        return "Task Success"
    "#;

    // 3. We begin the execution loop!
    let mut current_status = engine.execute(ai_script).unwrap();
    
    loop {
        match current_status {
            ExecutionResult::YieldedForTools { tools } => {
                println!("The AI suspended execution to run {} tool(s)...", tools.len());
                
                // 4. We execute Host actions concurrently (e.g. tokio::spawn handling API calls)
                let mut rust_responses = Vec::new();
                for tool in tools {
                    if tool.tool_name == "get_time" {
                        rust_responses.push(serde_json::json!({ "hour": "3 PM" }));
                    } else if tool.tool_name == "get_weather" {
                        rust_responses.push(serde_json::json!({ "condition": "Sunny" }));
                    }
                }
                
                // 5. We Wake Lua back up natively by passing the raw JSON in our loop!
                // The engine automatically deserializes it deep into the coroutine stack variables!
                current_status = engine.resume_with_json(rust_responses).unwrap();
            },
            ExecutionResult::Finished { ret_val, console_output } => {
                println!("Execution Finished Seamlessly! Returned: {:?}", ret_val);
                println!("Console Logs: \n{}", console_output);
                break;
            },
            ExecutionResult::Error(e) => panic!("Execution exception: {}", e),
        }
    }
}
```

## Available APIs
`AuwgentSandbox::new() -> Result<Self>`
Creates the completely secure, restricted Luau instance. Hooks custom limits and print bridges.

`engine.register_tools(tools: &[ToolDefinition]) -> Result<()>`
Generates native Lua wrapping functions allowing the AI to write beautiful synchronous code without stringifying raw JSON payloads. 

`engine.execute(script: &str) -> Result<ExecutionResult>`
Prepares and evaluates arbitrary chunks of code inside an exclusive `Thread`. The result will either hit a `YieldedForTools` or `Finished`.

`engine.resume_with_json(responses: Vec<serde_json::Value>) -> Result<ExecutionResult>`
Wake up a currently sleeping executor thread and hand Host JSON payloads smoothly back into the Lua script's scoped stack in milliseconds.

`engine.resume(args: MultiValue) -> Result<ExecutionResult>`
Wake up a currently sleeping executor thread and hand variables natively back deep into the Lua script's scoped stack in milliseconds.

`engine.get_console_output() -> String`
Get a dump of everything `print(...)`'d since the engine thread began.
