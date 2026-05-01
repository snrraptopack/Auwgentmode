/// Pure Lua computation — no tools, just data manipulation.
///
/// The input list is injected as a global variable `DATA`.
/// Expected answer: sort [3,1,4,1,5,9,2,6], deduplicate → [1,2,3,4,5,6,9],
/// take top 3 → [9,6,5], sum = 20.
///
/// Pass criteria:
/// - No tool rounds (pure computation)
/// - ret_val or console_output contains "20"
use auwgent_mode::ToolDefinition;

use crate::agent::{AgentRun, ScriptLanguage};
use crate::scenarios::{Scenario, ScenarioOutcome};

pub struct DataScenario;

impl Scenario for DataScenario {
    fn name(&self) -> &str {
        "data"
    }

    fn task(&self) -> &str {
        "The numbers are available in the Lua global DATA (a table / array). \
         Sort them in descending order, remove duplicates, then return the sum of \
         the top 3 numbers as a string."
    }

    fn task_for(&self, language: ScriptLanguage) -> String {
        match language {
            ScriptLanguage::Lua => self.task().to_string(),
            ScriptLanguage::JavaScript => {
                "The numbers are available in the JavaScript global DATA (an array). \
                 Sort them in descending order, remove duplicates, then console.log() \
                 the sum of the top 3 numbers as a string."
                    .to_string()
            }
        }
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        // No tools — this is pure Lua computation
        vec![]
    }

    fn globals(&self) -> Option<serde_json::Value> {
        // Inject the input list as a read-only global
        Some(serde_json::json!({ "DATA": [3, 1, 4, 1, 5, 9, 2, 6] }))
    }

    fn dispatch(&self, _tool_name: &str, _payload: &serde_json::Value) -> serde_json::Value {
        // Unreachable for this scenario, but must be implemented
        serde_json::json!({})
    }

    fn validate(&self, run: &AgentRun) -> ScenarioOutcome {
        if let Some(e) = &run.error {
            return ScenarioOutcome::Fail(format!("Engine/API error: {e}"));
        }

        if run.tool_rounds > 0 {
            return ScenarioOutcome::Warn(format!(
                "Model used {} tool round(s) for pure computation — unexpected",
                run.tool_rounds
            ));
        }

        // Correct answer: top-3 unique values of input are 9, 6, 5 → sum = 20
        let combined = run
            .ret_val
            .as_deref()
            .unwrap_or("")
            .trim()
            .to_string()
            + " "
            + &run.console_output;

        if combined.contains("20") {
            ScenarioOutcome::Pass(format!(
                "Correct answer (20) · ret_val={:?}",
                run.ret_val
            ))
        } else {
            ScenarioOutcome::Fail(format!(
                "Wrong result. Expected 20, got ret_val={:?} | output={}",
                run.ret_val,
                run.console_output.trim()
            ))
        }
    }
}
