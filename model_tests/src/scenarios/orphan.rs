/// Orphan detection stress test.
///
/// Runs the basic weather task N times and measures how often the model
/// forgets `await_all()`. The main runner accumulates `orphan_count` across
/// all runs and feeds a summary to the reporter — this `validate()` handles
/// single-run evaluation (used when `--rounds 1`).
use auwgent_mode::ToolDefinition;

use crate::agent::AgentRun;
use crate::scenarios::{make_tool, Scenario, ScenarioOutcome};

pub struct OrphanScenario;

impl Scenario for OrphanScenario {
    fn name(&self) -> &str {
        "orphan"
    }

    fn task(&self) -> &str {
        // Deliberately straightforward — we're testing model compliance, not task complexity.
        "What is the weather in Lagos? Print the result."
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![make_tool(
            "get_weather",
            "Returns current weather for a city",
            true,
            Some("{ location: string }"),
        )]
    }

    fn dispatch(&self, tool_name: &str, _payload: &serde_json::Value) -> serde_json::Value {
        match tool_name {
            "get_weather" => serde_json::json!({ "temp": "32C", "condition": "Sunny" }),
            other => serde_json::json!({ "__error": true, "message": format!("Unknown tool: {other}") }),
        }
    }

    fn validate(&self, run: &AgentRun) -> ScenarioOutcome {
        if let Some(e) = &run.error {
            return ScenarioOutcome::Fail(format!("Engine/API error: {e}"));
        }
        if run.orphaned_calls.is_empty() {
            ScenarioOutcome::Pass("No orphans — model used await_all() correctly".into())
        } else {
            ScenarioOutcome::Fail(format!(
                "Model forgot await_all() on: {}",
                run.orphaned_calls
                    .iter()
                    .map(|c| c.tool_name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        }
    }
}
