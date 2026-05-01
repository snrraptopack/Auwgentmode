/// Scenario trait and registry.
///
/// Each scenario defines:
/// - A user task description (sent as the user message to the LLM)
/// - The tools available in the sandbox for that task
/// - A mock dispatcher (returns fake results without real API calls)
/// - A validator that inspects the `AgentRun` and returns Pass / Fail / Warn
use auwgent_mode::ToolDefinition;

use crate::agent::{AgentRun, ScriptLanguage};

pub mod basic;
pub mod chained;
pub mod data;
pub mod orphan;
pub mod parallel;

// ── Outcome ───────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum ScenarioOutcome {
    /// Scenario passed; message describes the key result.
    Pass(String),
    /// Scenario failed; message explains why.
    Fail(String),
    /// Scenario completed but with a notable caveat (e.g. non-zero orphan rate).
    Warn(String),
}

// ── Trait ─────────────────────────────────────────────────────────────────────

pub trait Scenario {
    /// Short identifier used in CLI args and output (e.g. "basic", "parallel").
    fn name(&self) -> &str;

    /// The plain-English task description sent to the LLM.
    fn task(&self) -> &str;

    /// Language-specific task text. Override when the default mentions Lua-only
    /// constructs like `await_all()`.
    fn task_for(&self, _language: ScriptLanguage) -> String {
        self.task().to_string()
    }

    /// Tool stubs available to the script for this scenario.
    fn tools(&self) -> Vec<ToolDefinition>;

    /// Optional globals to inject before execution (e.g. input data tables).
    fn globals(&self) -> Option<serde_json::Value> {
        None
    }

    /// Return a mocked tool result for a given tool call.
    /// Called by the agent loop for every `YieldedForTools` round.
    fn dispatch(&self, tool_name: &str, payload: &serde_json::Value) -> serde_json::Value;

    /// Inspect the completed `AgentRun` and return Pass / Fail / Warn.
    fn validate(&self, run: &AgentRun) -> ScenarioOutcome;
}

// ── Registry ──────────────────────────────────────────────────────────────────

/// All scenarios in display order.
pub fn all_scenarios() -> Vec<Box<dyn Scenario>> {
    vec![
        Box::new(basic::BasicScenario),
        Box::new(parallel::ParallelScenario),
        Box::new(chained::ChainedScenario),
        Box::new(data::DataScenario),
        Box::new(orphan::OrphanScenario),
    ]
}

/// Look up a single scenario by its `name()`.
pub fn scenario_by_name(name: &str) -> Option<Box<dyn Scenario>> {
    all_scenarios().into_iter().find(|s| s.name() == name)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Convenience constructor for `ToolDefinition`.
pub fn make_tool(
    name:    &str,
    desc:    &str,
    has_args: bool,
    schema:  Option<&str>,
) -> ToolDefinition {
    ToolDefinition {
        name:       name.into(),
        description: desc.into(),
        has_args,
        arg_schema: schema.map(String::from),
    }
}
