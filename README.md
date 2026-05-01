# Auwgent Mode

A secure, high-performance in-process code execution sandbox for AI agents, written in Rust.

Instead of forcing your LLM into slow, single-turn JSON tool calls, Auwgent Mode lets the model write a single block of sandboxed Lua/Luau or JavaScript logic. The sandbox evaluates it in-process, routes tool calls to your Rust host, and feeds results back - all without a single LLM roundtrip during execution.

Inspired by [Pydantic's Monty](https://github.com/pydantic/monty). Built for production agentic systems.

---

## Why Auwgent Mode?

| Problem | Solution |
|---|---|
| LLMs call tools one at a time (slow) | Lua `await_all()` or JS `Promise.all()` batches tool calls into one yield |
| Untrusted code can crash your host | Luau `sandbox(true)` / QuickJS isolation + memory limits + instruction caps |
| Tool results need extra LLM turns to process | Lua or JavaScript manipulates results natively in the sandbox |
| JSON tool schemas cause hallucinations | Named-parameter stubs generated per tool |
| AI has no environment context | `inject_globals()` injects read-only agent variables |

---

## Engine Options

Auwgent Mode currently exposes two sandbox engines with the same host-facing execution model:

| Engine | Rust type | Model contract | Best fit |
|---|---|---|---|
| Luau/Lua | `AuwgentSandbox` / `LuauSandbox` | `await_all(tool(...))` | Existing Auwgent flows, strong coroutine semantics, orphan detection |
| QuickJS/JavaScript | `QuickJsSandbox` | `await tool(...)` and `Promise.all([...])` | Models and users that naturally write JavaScript, familiar data manipulation |

Both engines use the same shared types: `ToolDefinition`, `ExecutionResult`, `ToolResult`, `SandboxSnapshot`, and `ToolCall`. They also expose the same core methods: `register_tools()`, `inject_globals()`, `execute()`, `resume_with_json()`, `resume_with_results()`, `snapshot()`, and `from_snapshot()`.

---

## Architecture: Host-Controlled Tool Execution

This is the core design decision that makes Auwgent Mode fundamentally different from every other Lua sandbox or code-execution approach.

### What conventional sandboxes do

In most sandbox implementations, tools are **Rust functions bound directly inside the VM**. When the LLM calls `search_web()`, Lua synchronously invokes a real Rust closure — which may block the thread on an HTTP request, hold a database connection open, or directly touch your infrastructure.

```
[Lua VM]
  │
  └─► search_web()  →  [Rust function fires inside the VM]
                              │
                        HTTP request blocks here
                              │
                        Returns result to Lua
```

The VM is the executor. The host just watches.

### What Auwgent Mode does

In Auwgent Mode, the Lua function `search_web()` does **zero I/O**. It simply builds and returns a plain table describing *intent*:

```lua
-- This is ALL search_web() does inside the VM:
function search_web(args)
    return { name = "search_web", payload = args }
end
```

When the LLM calls `await_all(search_web(...))`, the sandbox immediately **freezes entirely** via `coroutine.yield()`. The resulting intent table is handed to the Rust host. The VM sits idle, consuming nothing.

```
[Lua VM]
  │
  └─► search_web({ query = "rust" })
          │
          └─► returns { name = "search_web", payload = { query = "rust" } }
                  │
          await_all() yields this to Rust ──► [VM is frozen]
                                                    │
                                             [Rust host owns it now]
                                                    │
                                          ┌─────────┴──────────────┐
                                          │ Authenticate the call   │
                                          │ Check rate limits       │
                                          │ Hit a cache layer       │
                                          │ Run tokio::join!()      │
                                          │ Log for audit trail     │
                                          │ Cancel if needed        │
                                          └─────────┬──────────────┘
                                                    │
                                          resume_with_json(result)
                                                    │
                                             [VM wakes up]
                                                    │
  local result = ...  ◄──────────────────────────────
```

### Why this matters

**The host has full sovereignty over every tool call.** The VM never touches your infrastructure directly. This enables capabilities that VM-bound tool systems cannot provide:

| Capability | VM-bound tools | Auwgent Mode |
|---|---|---|
| Parallel tool execution |  Sequential by default |  `tokio::join!` on every yield batch |
| Auth / permission checks | Baked into each tool fn |  Centralized in the host loop |
| Rate limiting | Per-function implementation |  One place in the host dispatch |
| Caching | Per-function implementation |  Intercept any tool by name |
| Audit logging | Per-function implementation |  Log every `ToolCall` struct |
| Cancellation | Cannot cancel a running Rust fn |  Drop the resume call |
| Tool mocking for tests | Requires VM setup |  Just return different JSON |

### What the host receives

When the LLM script yields, your Rust application receives a clean, typed vector of `ToolCall` structs:

```rust
pub struct ToolCall {
    pub tool_name: String,          // "search_web"
    pub payload: serde_json::Value, // { "query": "rust sandbox" }
}
```

Your host dispatch loop is the only place that runs real tools:

```rust
ExecutionResult::YieldedForTools { tools } => {
    // Run ALL yielded tools concurrently (parallel execution for free)
    let responses: Vec<serde_json::Value> = tools
        .iter()
        .map(|tool| dispatch_tool(tool))   // your logic here
        .collect();

    status = engine.resume_with_json(responses)?;
}
```

The result is a clean separation of concerns: **the LLM describes what it needs, Rust decides how and when to execute it.**

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

### Luau/Lua

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
                // Dispatch each tool by name — your real implementation
                // would call actual APIs here (tokio::join! for parallel)
                let responses: Vec<serde_json::Value> = tools
                    .iter()
                    .map(|t| match t.tool_name.as_str() {
                        "get_weather" => serde_json::json!({
                            "condition": "Sunny",
                            "temp": "32C"
                        }),
                        _ => serde_json::json!({ "error": "unknown tool" }),
                    })
                    .collect();
                status = engine.resume_with_json(responses).unwrap();
            }
            ExecutionResult::Finished { ret_val, console_output, orphaned_calls } => {
                // Feed the execution trace back to the LLM as context
                println!("Output:\n{}", console_output);

                // Use the structured return value in your application logic
                if let Some(val) = ret_val {
                    println!("Agent returned: {}", val);
                }

                // Detect tools the LLM called without await_all
                if !orphaned_calls.is_empty() {
                    println!("Warning: {} tool(s) were called but never executed:", orphaned_calls.len());
                    for c in &orphaned_calls {
                        println!("  - {} (payload: {})", c.tool_name, c.payload);
                    }
                    // Correct the LLM on the next turn (see Orphan Detection section)
                }
                break;
            }
            ExecutionResult::Error(e) => panic!("{}", e),
        }
    }
}
```

---

### QuickJS/JavaScript

```rust
use auwgent_mode::{ExecutionResult, QuickJsSandbox, ToolDefinition};

fn main() {
    // 1. Create the QuickJS sandbox
    let mut engine = QuickJsSandbox::new().unwrap();

    // 2. Register tools - auto-generates async JavaScript stubs
    engine.register_tools(&[
        ToolDefinition {
            name: "get_weather".into(),
            description: "Returns weather for a city".into(),
            has_args: true,
            arg_schema: Some("{ location: string }".into()),
        },
    ]).unwrap();

    // 3. The LLM writes JavaScript with top-level await support
    let script = r#"
        const weather = await get_weather({ location: "Lagos" });
        console.log("Condition:", weather.condition);
    "#;

    // 4. Drive the same host execution loop
    let mut status = engine.execute(script).unwrap();
    loop {
        match status {
            ExecutionResult::YieldedForTools { tools } => {
                let responses: Vec<serde_json::Value> = tools
                    .iter()
                    .map(|t| match t.tool_name.as_str() {
                        "get_weather" => serde_json::json!({
                            "condition": "Sunny",
                            "temp": "32C"
                        }),
                        _ => serde_json::json!({ "__error": true, "message": "unknown tool" }),
                    })
                    .collect();
                status = engine.resume_with_json(responses).unwrap();
            }
            ExecutionResult::Finished { console_output, .. } => {
                println!("Output:\n{}", console_output);
                break;
            }
            ExecutionResult::Error(e) => panic!("{}", e),
        }
    }
}
```

For independent JavaScript tool calls, ask the model to batch with `Promise.all()`:

```javascript
const [weather, stocks] = await Promise.all([
    get_weather({ location: "NY" }),
    get_stocks({ ticker: "AAPL" })
]);
console.log(weather.temp, stocks.price);
```

---

## Core Concepts

### The Execution Loop

Every Lua script runs inside a coroutine. When the LLM calls `await_all(...)`, the coroutine **yields** - fully suspending the script. JavaScript uses the same host-facing yield model through async tool promises: `await tool(...)` yields one tool call, and `Promise.all([...])` yields a batch. Your Rust application receives a `YieldedForTools` result, executes the tools (including async I/O), then calls `resume_with_json()` to wake the script back up with the results.

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
          Finished { console_output, ret_val, orphaned_calls }  ← done
```

The LLM is **not involved** during any of these steps. It wrote the script once and went to sleep.

### Parallel vs Sequential Tool Calls

**Parallel in Lua** - call multiple tools in one yield:
```lua
-- Both tools execute concurrently on the Rust side
local weather, stocks = await_all(
    get_weather({ location = "NY" }),
    get_stocks({ ticker = "AAPL" })
)
print(weather.temp, stocks.price)
```

**Parallel in JavaScript** - call independent tools through one `Promise.all()`:
```javascript
const [weather, stocks] = await Promise.all([
    get_weather({ location: "NY" }),
    get_stocks({ ticker: "AAPL" })
]);
console.log(weather.temp, stocks.price);
```

**Sequential** - yield multiple times, using previous results:
```lua
local user = await_all(fetch_user({ id = "123" }))

-- Use the user data from yield 1 to drive yield 2
local report = await_all(generate_report({ user_id = user.id }))
print(report.summary)
```

JavaScript sequential calls use normal `await`:

```javascript
const user = await fetch_user({ id: "123" });
const report = await generate_report({ user_id: user.id });
console.log(report.summary);
```

### Named Parameters (Single-Table Convention)

All tools accept a **single table/object** of named arguments. This prevents positional-argument hallucinations and maps cleanly to JSON.

```lua
--  Correct: named parameters in a table
local result = await_all(search({ query = "rust sandbox", limit = 10 }))

--  Avoid: positional args are fragile for LLMs
local result = await_all(search("rust sandbox", 10))
```

### Console Output

Everything `print()`'d by Lua or `console.log()`'d by JavaScript is captured in a Rust buffer and available in `ExecutionResult::Finished` as `console_output`. This is the **LLM's context channel** - it shows what the model's script did, step by step.

```rust
ExecutionResult::Finished { console_output, .. } => {
    // Append to conversation history so the model can see what happened
    llm.add_message("tool", &console_output);
}
```

Typical script output:
```
Fetching weather for Lagos...
Condition: Sunny, Temp: 32C
Analysis complete.
```

---

### Return Value (`ret_val`)

The `ret_val` field in `ExecutionResult::Finished` is the value the LLM script explicitly `return`s — the **host's action channel**. Unlike `console_output` (which is verbose and human-readable), `ret_val` is a compact, machine-readable signal your application code acts on directly.

```rust
ExecutionResult::Finished { ret_val, .. } => {
    match ret_val.as_deref() {
        Some("APPROVED")  => approve_request(session_id),
        Some("REJECTED")  => reject_request(session_id),
        Some("ESCALATE")  => notify_human(session_id),
        Some(val)         => store_result(session_id, val),
        None              => { /* script ran but returned nothing — fine */ }
    }
}
```

**When to use it:**

| Use case | What the script returns | What the host does |
|---|---|---|
| Routing / state machine | `"APPROVED"` / `"REJECTED"` | Triggers next workflow step |
| Data extraction | A customer ID, an amount, a status | Stores in database |
| Final answer to user | The computed result string | Displays in UI |
| Validation | `"valid"` / `"invalid:reason"` | Gates the next operation |
| Compound result | `"id\|total\|status"` | Splits and stores fields |

**Key distinction:**

```
console_output  →  the MODEL reads it on the next turn  (execution trace)
ret_val         →  your HOST APPLICATION reads it       (structured result)
```

Scripts do not have to return anything. If no `return` is executed, `ret_val` is `None` — this is normal for "do this task and print results" style agents.

---

## Full API Reference

The examples in this section use `AuwgentSandbox` for Luau/Lua. `QuickJsSandbox` mirrors the same host API unless noted: `new()`, `register_tools()`, `generate_tool_prompt()`, `inject_globals()`, `execute()`, `resume_with_json()`, `resume_with_results()`, `get_console_output()`, `snapshot()`, and `from_snapshot()`.

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

### `QuickJsSandbox::new() -> JsResult<Self>`

Creates a QuickJS VM with the same host-controlled tool-yield protocol.

```rust
use auwgent_mode::QuickJsSandbox;

let mut engine = QuickJsSandbox::new().unwrap();
```

QuickJS-specific behavior:
- Tool stubs are async JavaScript functions.
- Use `await tool({...})` for one tool call.
- Use `Promise.all([tool_a(), tool_b()])` to batch independent calls.
- `console.log()` is captured as `console_output`.
- The runtime has a 20 MB memory limit and an interrupt handler for infinite-loop protection.

---

### `engine.register_tools(tools: &[ToolDefinition]) -> LuaResult<()>`

Injects sandbox function stubs for every tool. In Lua these are plain functions that return intent tables; in JavaScript these are async functions that return Promises resolved by the host. Must be called **before** `execute()`.

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

The generated JavaScript stubs are used like:

```javascript
const weather = await get_weather({ location: "Lagos" });
const [weather, stocks] = await Promise.all([
    get_weather({ location: "Lagos" }),
    get_stocks({ ticker: "AAPL" })
]);
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

Injects read-only context variables into the sandbox global environment **before** the script runs. Must be called before `execute()`.

```rust
engine.inject_globals(serde_json::json!({
    "AGENT_ID":       "agent_007",
    "SESSION_ID":     "sess_abc123",
    "WORKSPACE_PATH": "/app/workspace",
    "USER_LOCALE":    "en-NG"
})).unwrap();
```

The LLM can then access these natively. In Lua:
```lua
print("Running as agent:", AGENT_ID)
local report = await_all(save_report({
    session = SESSION_ID,
    path    = WORKSPACE_PATH .. "/report.txt"
}))
```

In JavaScript:

```javascript
console.log("Running as agent:", AGENT_ID);
const report = await save_report({
    session: SESSION_ID,
    path: WORKSPACE_PATH + "/report.txt"
});
```

---

### `engine.execute(script: &str) -> LuaResult<ExecutionResult>`

Loads and starts executing a Lua script. On the **first call**, this locks the sandbox by applying `sandbox(true)` - freezing the global table so the LLM script cannot override tools or the `print` function. For `QuickJsSandbox`, `execute()` wraps the source in an async IIFE so top-level `await` works.

The instruction counter is **reset** on every `execute()` call, so re-using the engine for multiple sequential scripts is safe.

```rust
let result = engine.execute(llm_generated_script)?;
```

Returns one of:
- `ExecutionResult::Finished { ret_val, console_output, orphaned_calls }` — script ran to completion
- `ExecutionResult::YieldedForTools { tools }` — script is paused waiting for tool results
- `ExecutionResult::Error(String)` - non-recoverable sandbox runtime error

See the [Orphan Detection](#orphaned-tool-detection) section for how to handle `orphaned_calls`.

---

### `engine.resume_with_json(responses: Vec<serde_json::Value>) -> LuaResult<ExecutionResult>`

Resumes a suspended script, injecting tool results back into the Lua coroutine or JavaScript promises. The order of `responses` must match the order of tools in the last `YieldedForTools`.

Use this when **all tools are guaranteed to succeed**. For mixed success/failure batches, use `resume_with_results` instead.

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

### `engine.resume_with_results(results: Vec<ToolResult>) -> LuaResult<ExecutionResult>`

The production-grade alternative to `resume_with_json`. Accepts a mix of `ToolResult::Ok` and `ToolResult::Err` so the LLM can handle individual tool failures without crashing the entire script.

**`ToolResult` variants:**
- `ToolResult::Ok(serde_json::Value)` - injected as a normal Lua table or JavaScript object
- `ToolResult::Err(String)` - injected as `{ __error = true, message = "..." }` in Lua or `{ __error: true, message: "..." }` in JavaScript

```rust
ExecutionResult::YieldedForTools { tools } => {
    let results: Vec<ToolResult> = tools.iter().map(|tool| {
        match call_tool(&tool.tool_name, &tool.payload) {
            Ok(data)  => ToolResult::Ok(data),
            Err(e)    => ToolResult::Err(e.to_string()),
        }
    }).collect();

    status = engine.resume_with_results(results).unwrap();
}
```

The LLM checks failures using the `__error` sentinel field:

```lua
local weather, stocks, news = await_all(
    get_weather({ location = "NY" }),
    get_stocks({ ticker = "AAPL" }),
    get_news()
)

-- Handle individual failures without crashing
if stocks.__error then
    print("Stocks unavailable:", stocks.message)
else
    print("Price:", stocks.price)
end

-- Other results are unaffected
print("Weather:", weather.condition)
print("News:", news.headline)
```

> **Why `__error` and not `pcall`?**
> LLMs are generally unreliable at writing correct `pcall`/`xpcall` error handling in Lua.
> The `__error` sentinel mirrors the `{ "error": "..." }` pattern LLMs already know from REST API responses,
> making it far more likely the model will handle errors correctly and consistently.

---

### `engine.get_console_output() -> String`

Returns everything `print()`'d or `console.log()`'d so far in the current execution. Normally you read this from `ExecutionResult::Finished`, but this method lets you read it mid-execution if needed.

---

## Orphaned Tool Detection

Some LLM models occasionally write a Lua tool call **without** wrapping it in `await_all()`. Instead of silently discarding the call (and returning nothing to the script), the Luau engine tracks it and surfaces the intent in `ExecutionResult::Finished` so the host can correct the model.

For JavaScript, prompt models to `await` every tool call or use `Promise.all()` for independent tools. Unawaited JavaScript promises are still visible to the QuickJS bridge as pending tool calls, but the most reliable contract for model behavior is explicit `await`.

```lua
-- LLM bug: calls get_weather but forgets await_all
get_weather({ location = "Lagos" })   -- intent built, but never executed
return "done"
```

The script finishes without error — but no tool ran. Without detection, the host would never know the LLM intended to call a tool.

**With orphan detection**, the host gets:

```rust
ExecutionResult::Finished {
    ret_val: Some("done"),
    console_output: "",
    orphaned_calls: [
        ToolCall {
            tool_name: "get_weather",
            payload: { "location": "Lagos" },
        }
    ]
}
```

### Corrective feedback pattern

Use `orphaned_calls` to build a targeted corrective message for the next LLM turn:

```rust
ExecutionResult::Finished { orphaned_calls, console_output, ret_val } => {
    // Feed the execution trace back to the LLM
    llm.add_message("assistant", &console_output);

    if !orphaned_calls.is_empty() {
        // Build a precise correction instead of a generic retry
        let correction = orphaned_calls.iter().map(|c| {
            format!(
                "You called `{}` without `await_all()`. \
                 Wrap it like this: `await_all({}(...))` to actually execute it.",
                c.tool_name, c.tool_name
            )
        }).collect::<Vec<_>>().join("\n");

        llm.add_message("system", &correction);
        // LLM will self-correct on the next turn
    }
}
```

### Why this beats a generic retry

| Without orphan detection | With orphan detection |
|---|---|
| Script finishes silently | Host knows exactly which tool(s) were intended |
| Host sends a vague "try again" | Host sends: *"You called `get_weather` without `await_all`"* |
| LLM may repeat the same mistake | LLM gets tool name + payload — can fix with precision |
| Payload is lost | Payload is preserved — corrective message can include it |

> **How it works internally:** Each tool stub registers its intent in a Rust-owned `HashMap` (bypassing the Lua sandbox restriction on table mutation). When `await_all()` yields an intent to Rust, that entry is removed. Whatever remains in the map when the script finishes was never yielded — those are the orphaned calls.

---

## Session Persistence & Snapshots

One of the hardest problems in agentic backends is keeping an LLM's execution session alive across stateless HTTP requests. Auwgent Mode solves this with `SandboxSnapshot` — a serializable record of everything needed to rebuild the engine and fast-forward it to exactly where it left off.

### How it works

Instead of serializing the raw Lua VM (which is not portable), the snapshot records the **complete history** of the session:

```
execute(script)                      ← round 0 yielded
resume_with_json(results_0)          ← round 1 yielded   [server could die here]
resume_with_json(results_1)          ← round 2 yielded   [or here, or anywhere]
snapshot() → SandboxSnapshot {
    script_source:           "...",
    completed_tool_results:  [results_0, results_1],
    tool_definitions:        [...],
    injected_globals:        {...},
    libraries:               [...],
}
```

On restore, a **new VM is built**, the script runs from the top, but every yield that already has a cached result is fast-forwarded by injecting the stored JSON — no real tools are called. The engine stops and returns control at the **first yield with no cached result**.

```
from_snapshot(snap)
  → new VM, register tools, inject globals, load libraries
  → execute(script) → YieldedForTools  // round 0
  → inject results_0 immediately        // fast-forward (no real tool call)
  → YieldedForTools                     // round 1
  → inject results_1 immediately        // fast-forward
  → YieldedForTools                     // round 2 ← returned to caller
```

### `engine.snapshot() -> Option<SandboxSnapshot>`

Capture the current session state. Returns `None` if `execute()` has not been called yet. State is tracked automatically — no manual effort needed from the caller.

```rust
// After resolving some yields...
let snap = engine.snapshot().unwrap();

// Serialize to string for storage (Postgres, Redis, file, etc.)
let json = serde_json::to_string(&snap).unwrap();
db.store("session:agent_007", &json).await?;
```

### `AuwgentSandbox::from_snapshot(snap) -> LuaResult<(Self, ExecutionResult)>`

Restore an engine from a snapshot. Returns the rebuilt engine and the `ExecutionResult` at the restored position — which will be either the next un-resolved `YieldedForTools` or `Finished` if all yields were already cached.

```rust
let json = db.load("session:agent_007").await?;
let snap: SandboxSnapshot = serde_json::from_str(&json).unwrap();

let (mut engine, status) = AuwgentSandbox::from_snapshot(snap).unwrap();

// status is already at the next live yield — resume normally
match status {
    ExecutionResult::YieldedForTools { tools } => {
        let responses = dispatch_tools(&tools).await;
        engine.resume_with_json(responses)?;
    }
    ExecutionResult::Finished { orphaned_calls, .. } => {
        // already done — check for orphans if needed
        if !orphaned_calls.is_empty() { /* handle */ }
    }
    ExecutionResult::Error(e) => { panic!("{}", e); }
}
```

### Real-world pattern: Stateless HTTP backend

```rust
// POST /agent/resume  { session_id, tool_results }
async fn resume_handler(session_id: &str, results: Vec<serde_json::Value>) {
    // 1. Load snapshot from storage
    let json = db.load(session_id).await?;
    let snap: SandboxSnapshot = serde_json::from_str(&json)?;

    // 2. Restore engine at the correct yield point (fast-forward is automatic)
    let (mut engine, _) = AuwgentSandbox::from_snapshot(snap)?;

    // 3. Inject the new tool results
    let status = engine.resume_with_json(results)?;

    // 4. Save updated snapshot for the next request
    let new_snap = engine.snapshot().unwrap();
    db.store(session_id, &serde_json::to_string(&new_snap)?).await?;

    // 5. Return the next yield to the frontend
    send_response(status);
}
```

> **Determinism requirement:** This pattern assumes the LLM script is deterministic — i.e. the same script run twice with the same inputs produces the same sequence of yields. Avoid `os.clock()` or any non-deterministic Lua globals in scripts that will be snapshotted.

### `engine.load_library(lua_code: &str) -> LuaResult<()>`

Load reusable Lua utility functions before execution. Unlike `register_tools`, library functions run entirely inside the VM — they do not yield to Rust and have no tool overhead.

**Key benefit:** The LLM never needs to rewrite utility logic. Just load it once per engine, and the model can call it anywhere in its script.

```rust
engine.load_library(r#"
    function format_currency(amount, symbol)
        return symbol .. string.format("%.2f", amount)
    end

    function clamp(val, min_val, max_val)
        return math.max(min_val, math.min(max_val, val))
    end

    function avg(list)
        local total = 0
        for _, v in ipairs(list) do total = total + v end
        return total / #list
    end
"#).unwrap();
```

The LLM calls these as regular functions:

```lua
local prices = await_all(get_prices())
local formatted = format_currency(prices.latest, "$")
local smoothed   = clamp(prices.index, 0, 100)
print("Price:", formatted, "Index:", smoothed)
```

Library code is automatically stored in `SandboxSnapshot.libraries` and reloaded on `from_snapshot()`, so utility functions are always available after restore.

---

## Security Model

| Threat | Luau mitigation | QuickJS mitigation |
|---|---|---|
| File system access | `io`, `os`, `package`, `debug` libraries not loaded | No host file APIs are exposed |
| External network calls | No socket/HTTP primitives available | No `fetch`, `require`, imports, or host network APIs are exposed |
| Infinite loops | Instruction interrupt kills runaway scripts | Runtime interrupt handler kills runaway scripts |
| Memory bombs | Hard 20 MB heap limit via mlua allocator hook | Hard 20 MB QuickJS runtime memory limit |
| Overriding host stubs | `sandbox(true)` freezes globals before first execute | Host only drains registered resolver queue and Rust-owned state |
| Accessing host Rust state | All host data must be explicitly passed via `resume_with_json()` | Same: all host data must be explicitly passed via `resume_with_json()` |

---

## Live Model Tests

The `model_tests` workspace member runs live Groq-backed model scenarios against either sandbox:

```bash
# Luau/Lua live tests (default)
cargo run --release -p model_tests

# QuickJS/JavaScript live tests
cargo run --release -p model_tests -- --engine js

# Inspect generated code
cargo run --release -p model_tests -- --engine js --scenario parallel --verbose
```

The same scenario validators are used for both engines. Lua prompts require `await_all(...)`; JavaScript prompts require `await` and `Promise.all(...)`.

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

## JavaScript Patterns for Agents

#### List Filtering
```javascript
const users = await get_users();
const active = users.filter((u) => u.is_active);
console.log("Active users:", active.length);
```

#### Sorting
```javascript
const products = await get_products();
products.sort((a, b) => a.price - b.price);
console.log("Cheapest:", products[0].name);
```

#### Aggregation
```javascript
const scores = await get_scores();
const total = scores.reduce((sum, n) => sum + n, 0);
console.log("Average:", total / scores.length);
```

#### Multi-Step Reasoning
```javascript
const user = await fetch_user({ id: USER_ID });
const fullName = `${user.first} ${user.last}`;
const recommendations = await recommend({ name: fullName, age: user.age });
for (const item of recommendations) {
    console.log("-", item.title);
}
```

#### Parallel Tools
```javascript
const [weather, stocks] = await Promise.all([
    get_weather({ location: "NY" }),
    get_stocks({ ticker: "AAPL" })
]);
console.log(weather.condition, stocks.price);
```

---

## Project Structure

```
Auwgentmode/
|-- src/
|   |-- lib.rs          # Public exports
|   |-- luau_engine.rs  # Luau/Lua sandbox implementation
|   |-- js_engine.rs    # QuickJS/JavaScript sandbox implementation
|   |-- types.rs        # Shared public types
|   `-- tests.rs        # Luau unit tests
|-- tests/
|   |-- js_sandbox.rs         # QuickJS integration tests
|   `-- data_manipulation.rs  # Luau integration tests
|-- model_tests/              # Live Groq model tests for Lua and JS
|-- Cargo.toml
`-- README.md
```

---

## Contributing

The engine is designed to be extended, not modified. The recommended pattern is:

1. Add new `ToolDefinition` variants - the engines handle stub generation automatically.
2. Add new globals via `inject_globals()` - no engine changes needed.
3. To add new security primitives (e.g. network rate limits), hook into the host dispatch layer or the relevant engine implementation.

When adding features, ensure every new behaviour has a corresponding test in either `src/tests.rs` (unit) or `tests/` (integration).
