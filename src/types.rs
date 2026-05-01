use serde::{Deserialize, Serialize};

// ─── Tool Call ───────────────────────────────────────────────────────────────

/// A tool call intent yielded by the LLM script, ready for Rust to execute.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub tool_name: String,
    pub payload: serde_json::Value,
}

// ─── Tool Definition ─────────────────────────────────────────────────────────

/// Describes a tool the LLM is allowed to call.
///
/// Used to register tools AND auto-generate system prompt descriptions.
/// Derives Serialize/Deserialize so it can be stored in a SandboxSnapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    /// Human-readable description of what this tool does (used in prompt generation).
    pub description: String,
    /// Whether this tool accepts a named-parameter object as its single argument.
    pub has_args: bool,
    /// Optional schema hint shown to the LLM, e.g. `{ location: string, units: string }`.
    pub arg_schema: Option<String>,
}

// ─── Execution Result ────────────────────────────────────────────────────────

/// The result of a single sandbox execution step.
#[derive(Debug)]
pub enum ExecutionResult {
    /// The script completed normally.
    Finished {
        ret_val: Option<String>,
        console_output: String,
        /// Tool intents built by stubs but never properly awaited.
        ///
        /// For Luau: tools called without `await_all(...)`.
        /// For JS: tool Promises created but never `await`ed.
        ///
        /// Feed these back to the LLM as corrective messages so it
        /// self-corrects on the next turn without crashing.
        orphaned_calls: Vec<ToolCall>,
    },
    /// The script paused and is waiting for these host tools to execute.
    YieldedForTools { tools: Vec<ToolCall> },
    /// A non-recoverable runtime error occurred inside the script.
    Error(String),
}

// ─── Tool Result ─────────────────────────────────────────────────────────────

/// The outcome of a single host-side tool execution.
///
/// Use this with `resume_with_results()` to give the LLM structured
/// success or failure feedback without crashing the entire script.
#[derive(Debug)]
pub enum ToolResult {
    /// The tool succeeded. The value is injected as a native object into the script.
    Ok(serde_json::Value),
    /// The tool failed. Injected as `{ __error: true, message: "..." }` so the
    /// LLM can check `if result.__error` and handle gracefully.
    Err(String),
}

// ─── Sandbox Snapshot ────────────────────────────────────────────────────────

/// A serializable point-in-time snapshot of a sandbox execution session.
///
/// Stores everything needed to reconstruct the engine and fast-forward
/// it to exactly the yield point it was at when the snapshot was taken.
/// This allows resuming execution across process restarts, HTTP request
/// boundaries, or any other stateless host environment.
///
/// # How it works
///
/// Rather than serializing raw VM state, the snapshot records the complete
/// *history* of the session: the original script, all completed tool result
/// rounds, and the registration configuration. On restore, a new VM replays
/// the script but automatically fast-forwards through every previously-resolved
/// yield — injecting cached results instead of calling real tools —
/// until it reaches the first *new* yield that needs real execution.
///
/// `ToolResult::Err` results are materialized as
/// `{ "__error": true, "message": "..." }` JSON before storage so that
/// replay via `resume_with_json` produces an identical value in both engines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxSnapshot {
    /// The original script source exactly as passed to `execute()`.
    pub script_source: String,
    /// All tool result rounds that have already been resolved and injected,
    /// in order. Each inner Vec corresponds to one `resume_with_results` call.
    pub completed_tool_results: Vec<Vec<serde_json::Value>>,
    /// The tool definitions registered before execution.
    pub tool_definitions: Vec<ToolDefinition>,
    /// The globals injected before execution via `inject_globals()`.
    pub injected_globals: serde_json::Value,
    /// Any library code loaded via `load_library()` before execution.
    pub libraries: Vec<String>,
}
