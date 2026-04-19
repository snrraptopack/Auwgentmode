/// Sequential chained calls: fetch a user, then use their ID in a second call.
///
/// Pass criteria:
/// - No orphaned calls
/// - At least 2 tool rounds (model chained the calls with data dependency)
/// - Output references the report or user name
use auwgent_mode::ToolDefinition;

use crate::agent::AgentRun;
use crate::scenarios::{make_tool, Scenario, ScenarioOutcome};

pub struct ChainedScenario;

impl Scenario for ChainedScenario {
    fn name(&self) -> &str {
        "chained"
    }

    fn task(&self) -> &str {
        "Fetch the user with ID 'usr_42'. \
         Then use their user ID to generate a summary report for them. \
         Print the report summary and return it."
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![
            make_tool(
                "fetch_user",
                "Fetch a user record by their ID",
                true,
                Some("{ id: string }"),
            ),
            make_tool(
                "generate_report",
                "Generate a summary report for a user by their ID",
                true,
                Some("{ user_id: string }"),
            ),
        ]
    }

    fn dispatch(&self, tool_name: &str, payload: &serde_json::Value) -> serde_json::Value {
        match tool_name {
            "fetch_user" => serde_json::json!({
                "id":   payload["id"].as_str().unwrap_or("usr_42"),
                "name": "Amara Okonkwo",
                "plan": "pro",
                "age":  29
            }),
            "generate_report" => serde_json::json!({
                "user_id": payload.get("user_id").cloned().unwrap_or_default(),
                "summary": "Amara Okonkwo (Pro plan) — 12 active projects, last login 2h ago."
            }),
            other => serde_json::json!({ "__error": true, "message": format!("Unknown tool: {other}") }),
        }
    }

    fn validate(&self, run: &AgentRun) -> ScenarioOutcome {
        if let Some(e) = &run.error {
            return ScenarioOutcome::Fail(format!("Engine/API error: {e}"));
        }
        if !run.orphaned_calls.is_empty() {
            return ScenarioOutcome::Fail(format!(
                "Orphaned calls: {}",
                run.orphaned_calls.iter().map(|c| c.tool_name.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
        if run.tool_rounds < 2 {
            return ScenarioOutcome::Fail(format!(
                "Expected ≥2 sequential rounds, got {}. \
                 Model may have collapsed both calls into one batch (missed data dependency).",
                run.tool_rounds
            ));
        }

        // 2+ rounds is the definitive structural proof that the model chained
        // the calls correctly. We also verify the output contains the injected
        // mock data to ensure the table serialization works.
        let combined = run.console_output.to_lowercase()
            + run.ret_val.as_deref().unwrap_or("").to_lowercase().as_str();

        if combined.contains("amara") || combined.contains("report") || combined.contains("pro") {
            ScenarioOutcome::Pass(format!(
                "{} sequential rounds — output correctly references user/report data",
                run.tool_rounds
            ))
        } else {
            ScenarioOutcome::Warn(format!(
                "{} rounds ran but output doesn't mention expected user data. Output: {}",
                run.tool_rounds,
                combined.chars().take(80).collect::<String>().replace('\n', " ")
            ))
        }
    }
}
