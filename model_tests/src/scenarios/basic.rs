/// Single tool call: `get_weather` for Lagos.
///
/// Pass criteria:
/// - Model used `await_all()` — no orphaned calls
/// - At least one tool round occurred
/// - Output or return value references recognisable weather data
use auwgent_mode::ToolDefinition;

use crate::agent::AgentRun;
use crate::scenarios::{make_tool, Scenario, ScenarioOutcome};

pub struct BasicScenario;

impl Scenario for BasicScenario {
    fn name(&self) -> &str {
        "basic"
    }

    fn task(&self) -> &str {
        "What is the weather in Lagos right now? \
         Print the temperature and condition, then return the condition as a string."
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
            "get_weather" => serde_json::json!({
                "temp":      "32C",
                "condition": "Sunny",
                "humidity":  "78%"
            }),
            other => serde_json::json!({ "__error": true, "message": format!("Unknown tool: {other}") }),
        }
    }

    fn validate(&self, run: &AgentRun) -> ScenarioOutcome {
        if let Some(e) = &run.error {
            return ScenarioOutcome::Fail(format!("Engine/API error: {e}"));
        }
        if !run.orphaned_calls.is_empty() {
            let names: Vec<&str> = run.orphaned_calls.iter().map(|c| c.tool_name.as_str()).collect();
            return ScenarioOutcome::Fail(format!(
                "Model forgot await_all on: {}",
                names.join(", ")
            ));
        }
        if run.tool_rounds == 0 {
            return ScenarioOutcome::Fail(
                "No tools were called — model skipped execution entirely".into(),
            );
        }

        // Check whether weather data appears in output or return value
        let combined = run.console_output.to_lowercase()
            + run.ret_val.as_deref().unwrap_or("").to_lowercase().as_str();

        if combined.contains("sunny") || combined.contains("32") || combined.contains("weather") {
            ScenarioOutcome::Pass(format!(
                "ret_val={:?} · output={} chars",
                run.ret_val,
                run.console_output.len()
            ))
        } else {
            ScenarioOutcome::Warn(
                "Tool executed but output doesn't mention expected weather data — run with --verbose".into(),
            )
        }
    }
}
