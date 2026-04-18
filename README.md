# Auwgent Mode

A secure, high-performance in-process Lua execution sandbox for AI agents, written in Rust.

Instead of forcing your LLM into slow, single-turn JSON tool calls, Auwgent Mode lets the model write a single block of Lua logic. The sandbox evaluates it in microseconds, routes tool calls to your Rust host, and feeds results back — all without a single LLM roundtrip during execution.

Inspired by [Pydantic's Monty](https://github.com/pydantic/monty). Built for production agentic systems.

---

## Why Auwgent Mode?

| Problem | Solution |
|---|---|
| LLMs call tools one at a time (slow) | `await_all()` batches tool calls into one yield |
| Untrusted code can crash your host | Luau `sandbox(true)` + memory limits + instruction caps |
| Tool results need extra LLM turns to process | Lua manipulates results natively in the sandbox |
| JSON tool schemas cause hallucinations | Named-parameter stubs generated per tool |
| AI has no environment context | `inject_globals()` injects read-only agent variables |

---

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
auwgent_mode = { path = "../Auwgentmode" }   # local path
serde_json = "1"
```

> When published to crates.io, replace the path with `version = "x.y.z"`.

---

## Quick Start

```rust
use auwgent_mode::{AuwgentSandbox, ExecutionResult, ToolDefinition};

fn main() {
    // 1. Create the sandbox
    let mut engine = AuwgentSandbox::new().unwrap();

    // 2. Register tools — auto-generates Lua stubs for the LLM
    engine.register_tools(&[
        ToolDefinition {
            name: "get_weather".into(),
            description: "Returns weather for a city".into(),
            has_args: true,
            arg_schema: Some("{ location: string }".into()),
        },
    ]).unwrap();

    // 3. The LLM writes this — no JSON, no stringify, pure Lua
    let script = r#"
        local weather = await_all(get_weather({ location = "Lagos" }))
        print("Condition:", weather.condition)
        return weather.condition
    "#;

    // 4. Drive the execution loop
    let mut status = engine.execute(script).unwrap();
    loop {
        match status {
            ExecutionResult::YieldedForTools { tools } => {
                let responses = tools.iter().map(|t| {
                    // Run your actual API call here
                    serde_json::json!({ "condition": "Sunny", "temp": "32C" })
                }).collect();
                status = engine.resume_with_json(responses).unwrap();
            }
            ExecutionResult::Finished { ret_val, console_output } => {
                println!("Output:\n{}", console_output);
                println!("Returned: {:?}", ret_val);
                break;
            }
            ExecutionResult::Error(e) => panic!("{}", e),
        }
    }
}
```

---

## Core Concepts

### The Execution Loop

Every script runs inside a Lua coroutine. When the LLM calls `await_all(...)`, the coroutine **yields** — fully suspending the script. Your Rust application receives a `YieldedForTools` result, executes the tools (including async I/O), then calls `resume_with_json()` to wake the script back up with the results.

```
LLM generates script once
    │
    ▼
engine.execute(script)
    │
    ├─► YieldedForTools { tools }  ← script is frozen here
    │         │
    │    [your Rust code runs APIs]
    │         │
    │    engine.resume_with_json(responses)
    │         │
    └─► YieldedForTools { tools }  ← script can yield again
              │
         [more Rust API work]
              │
         engine.resume_with_json(...)
              │
         Finished { console_output, ret_val }  ← done
```

The LLM is **not involved** during any of these steps. It wrote the script once and went to sleep.

### Parallel vs Sequential Tool Calls

**Parallel** — call multiple tools in one yield:
```lua
-- Both tools execute concurrently on the Rust side
local weather, stocks = await_all(
    get_weather({ location = "NY" }),
    get_stocks({ ticker = "AAPL" })
)
print(weather.temp, stocks.price)
```

**Sequential** — yield multiple times, using previous results:
```lua
local user = await_all(fetch_user({ id = "123" }))

-- Use the user data from yield 1 to drive yield 2
local report = await_all(generate_report({ user_id = user.id }))
print(report.summary)
```

### Named Parameters (Single-Table Convention)

All tools accept a **single table** of named arguments. This prevents positional-argument hallucinations and maps cleanly to JSON.

```lua
-- ✅ Correct: named parameters in a table
local result = await_all(search({ query = "rust sandbox", limit = 10 }))

-- ❌ Avoid: positional args are fragile for LLMs
local result = await_all(search("rust sandbox", 10))
```

### Console Output

Everything `print()`'d by the LLM is captured in a Rust buffer. This is your **feedback loop** — pass `console_output` back to the LLM context so the model can see what its script did.

```rust
ExecutionResult::Finished { console_output, .. } => {
    // Feed this back into the LLM message history
    llm.add_message("tool", &console_output);
}
```

---

## Full API Reference

### `AuwgentSandbox::new() -> LuaResult<Self>`

Creates a secure Luau VM. The sandbox is **not locked** at this point — you can still register tools and inject globals.

```rust
let mut engine = AuwgentSandbox::new().unwrap();
// or equivalently:
let engine = AuwgentSandbox::default();
```

Security guarantees applied at construction:
- `os`, `io`, `package`, `debug` standard libraries are completely absent
- Memory capped at **20 MB**
- Instruction interrupt installed (infinite loop protection)

---

### `engine.register_tools(tools: &[ToolDefinition]) -> LuaResult<()>`

Injects Lua function stubs for every tool. Must be called **before** `execute()`.

```rust
engine.register_tools(&[
    ToolDefinition {
        name: "search_web".into(),
        description: "Searches the web and returns top results".into(),
        has_args: true,
        arg_schema: Some("{ query: string, limit: number }".into()),
    },
    ToolDefinition {
        name: "get_timestamp".into(),
        description: "Returns the current UTC timestamp".into(),
        has_args: false,
        arg_schema: None,
    },
]).unwrap();
```

The generated Lua stubs look like:
```lua
-- has_args = true
function search_web(args)
    return { name = "search_web", payload = args }
end

-- has_args = false
function get_timestamp()
    return { name = "get_timestamp" }
end
```

---

### `AuwgentSandbox::generate_tool_prompt(tools: &[ToolDefinition]) -> String`

Generates a system prompt block describing all tools. Feed this directly into your LLM system message.

```rust
let tools = vec![/* ... */];
engine.register_tools(&tools).unwrap();

// Build the system prompt from the same definitions
let tool_section = AuwgentSandbox::generate_tool_prompt(&tools);
let system_prompt = format!(
    "You are an AI agent. Write Lua code to complete tasks.\n\n{}",
    tool_section
);
```

Output example:
```
You have access to the following tools:

- `search_web`: Searches the web and returns top results Args: `{ query: string, limit: number }`
- `get_timestamp`: Returns the current UTC timestamp (no arguments)
```

---

### `engine.inject_globals(ctx: serde_json::Value) -> LuaResult<()>`

Injects read-only context variables into the Lua environment **before** the script runs. Must be called before `execute()`.

```rust
engine.inject_globals(serde_json::json!({
    "AGENT_ID":       "agent_007",
    "SESSION_ID":     "sess_abc123",
    "WORKSPACE_PATH": "/app/workspace",
    "USER_LOCALE":    "en-NG"
})).unwrap();
```

The LLM can then access these natively:
```lua
print("Running as agent:", AGENT_ID)
local report = await_all(save_report({
    session = SESSION_ID,
    path    = WORKSPACE_PATH .. "/report.txt"
}))
```

---

### `engine.execute(script: &str) -> LuaResult<ExecutionResult>`

Loads and starts executing a Lua script. On the **first call**, this locks the sandbox by applying `sandbox(true)` — freezing the global table so the LLM script cannot override tools or the `print` function.

The instruction counter is **reset** on every `execute()` call, so re-using the engine for multiple sequential scripts is safe.

```rust
let result = engine.execute(llm_generated_script)?;
```

Returns one of:
- `ExecutionResult::Finished { ret_val, console_output }` — script completed
- `ExecutionResult::YieldedForTools { tools }` — script is paused, needs tool results
- `ExecutionResult::Error(String)` — non-recoverable Lua runtime error

---

### `engine.resume_with_json(responses: Vec<serde_json::Value>) -> LuaResult<ExecutionResult>`

Resumes a suspended script, injecting tool results back into the Lua coroutine. The order of `responses` must match the order of tools in the last `YieldedForTools`.

```rust
ExecutionResult::YieldedForTools { tools } => {
    let mut responses = Vec::new();
    for tool in &tools {
        let result = match tool.tool_name.as_str() {
            "get_weather" => fetch_weather(&tool.payload).await,
            "get_stocks"  => fetch_stocks(&tool.payload).await,
            name => panic!("Unknown tool: {}", name),
        };
        responses.push(result);
    }
    status = engine.resume_with_json(responses).unwrap();
}
```

---

### `engine.get_console_output() -> String`

Returns everything `print()`'d so far in the current execution. Normally you read this from `ExecutionResult::Finished`, but this method lets you read it mid-execution if needed.

---

## Security Model

| Threat | Mitigation |
|---|---|
| File system access | `io`, `os` libraries not loaded |
| External network calls | No socket/HTTP primitives available |
| Infinite loops | Instruction interrupt fires every ~N ops; kills script after 100,000 pings |
| Memory bombs | Hard 20 MB heap limit via mlua allocator hook |
| Overriding `print`/`await_all` | `sandbox(true)` freezes globals before first execute |
| Accessing host Rust state | All host data must be explicitly passed via `resume_with_json()` |

---

## Lua Patterns for Agents

#### List Filtering
```lua
local users = await_all(get_users())
local active = {}
for _, u in ipairs(users) do
    if u.is_active then table.insert(active, u) end
end
print("Active users:", #active)
```

#### Sorting
```lua
local products = await_all(get_products())
table.sort(products, function(a, b) return a.price < b.price end)
print("Cheapest:", products[1].name)
```

#### Aggregation
```lua
local scores = await_all(get_scores())
local total = 0
for _, s in ipairs(scores) do total = total + s end
print("Average:", total / #scores)
```

#### Multi-Step Reasoning
```lua
-- Step 1: get data
local user = await_all(fetch_user({ id = USER_ID }))

-- Lua manipulation (free, no tool call needed)
local full_name = user.first .. " " .. user.last

-- Step 2: use derived data in next tool
local recommendations = await_all(recommend({ name = full_name, age = user.age }))
for _, r in ipairs(recommendations) do
    print("-", r.title)
end
```

#### Group By
```lua
local orders = await_all(get_orders())
local by_category = {}
for _, o in ipairs(orders) do
    by_category[o.category] = (by_category[o.category] or 0) + 1
end
print("Electronics:", by_category["Electronics"])
```

---

## Project Structure

```
Auwgentmode/
├── src/
│   ├── lib.rs          # Public exports
│   ├── engine.rs       # Core sandbox implementation
│   └── tests.rs        # Unit tests (security, API surface)
├── tests/
│   └── data_manipulation.rs  # Integration tests (list ops, data flows)
├── Cargo.toml
└── README.md
```

---

## Contributing

The engine is designed to be extended, not modified. The recommended pattern is:

1. Add new `ToolDefinition` variants — the engine handles stub generation automatically.
2. Add new globals via `inject_globals()` — no engine changes needed.
3. To add new security primitives (e.g. network rate limits), hook into the `resume_internal` method in `engine.rs`.

When adding features, ensure every new behaviour has a corresponding test in either `src/tests.rs` (unit) or `tests/` (integration).
