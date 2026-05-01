// ─── Shared types ─────────────────────────────────────────────────────────────
pub mod types;
pub use types::{ExecutionResult, SandboxSnapshot, ToolCall, ToolDefinition, ToolResult};

// ─── Luau (Luau/Lua) sandbox ──────────────────────────────────────────────────
pub mod luau_engine;
pub use luau_engine::LuauSandbox;

/// Backward-compatible alias — existing code using `AuwgentSandbox` still compiles.
pub type AuwgentSandbox = LuauSandbox;

// ─── QuickJS (JavaScript) sandbox ─────────────────────────────────────────────
pub mod js_engine;
pub use js_engine::{QuickJsSandbox, JsError};

// ─── Unit tests (Luau engine) ─────────────────────────────────────────────────
#[cfg(test)]
mod tests;
