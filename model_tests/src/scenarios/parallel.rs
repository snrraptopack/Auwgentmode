/// Parallel batch: `get_weather` + `get_stocks` in a single `await_all()`.
///
/// Pass criteria:
/// - No orphaned calls
/// - Both tools appear in one `YieldedForTools` round (true parallel batch)
/// - Warns if they ran in separate rounds (model chose sequential over parallel)
use auwgent_mode::ToolDefinition;

use crate::agent::AgentRun;
use crate::scenarios::{make_tool, Scenario, ScenarioOutcome};

pub struct ParallelScenario;

impl Scenario for ParallelScenario {
    fn name(&self) -> &str {
        "parallel"
    }

    fn task(&self) -> &str {
        "Get the weather in Lagos AND the stock price of AAPL at the same time \
         using a single await_all() call. Print both results."
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![
            make_tool(
                "get_weather",
                "Returns current weather for a city",
                true,
                Some("{ location: string }"),
            ),
            make_tool(
                "get_stocks",
                "Returns the current stock price for a ticker symbol",
                true,
                Some("{ ticker: string }"),
            ),
        ]
    }

    fn dispatch(&self, tool_name: &str, _payload: &serde_json::Value) -> serde_json::Value {
        match tool_name {
            "get_weather" => serde_json::json!({ "temp": "32C", "condition": "Sunny" }),
            "get_stocks"  => serde_json::json!({ "ticker": "AAPL", "price": "$189.30" }),
            other => serde_json::json!({ "__error": true, "message": format!("Unknown tool: {other}") }),
        }
    }

    fn validate(&self, run: &AgentRun) -> ScenarioOutcome {
        if let Some(e) = &run.error {
            return ScenarioOutcome::Fail(format!("Engine/API error: {e}"));
        }
        if !run.orphaned_calls.is_empty() {
            return ScenarioOutcome::Fail(format!(
                "Model forgot await_all on: {}",
                run.orphaned_calls.iter().map(|c| c.tool_name.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
        if run.tool_rounds == 0 {
            return ScenarioOutcome::Fail("No tools called at all".into());
        }

        if run.tool_rounds == 1 {
            ScenarioOutcome::Pass(
                "Both tools batched in one await_all() (true parallel yield)".into(),
            )
        } else {
            // Executed correctly but not batched — warn rather than fail
            ScenarioOutcome::Warn(format!(
                "Tools ran in {} sequential rounds — model didn't batch them in one await_all()",
                run.tool_rounds
            ))
        }
    }
}
