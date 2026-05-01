/// Groq API client using the OpenAI-compatible chat completions endpoint.
///
/// Uses `reqwest::blocking` so the test binary stays synchronous — no async
/// runtime needed when the engine itself is also fully synchronous.
use serde::{Deserialize, Serialize};

/// Groq's OpenAI-compatible base URL.
const GROQ_BASE_URL: &str = "https://api.groq.com/openai/v1";

// ── Request types ─────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ChatRequest<'a> {
    model:       &'a str,
    messages:    &'a [Message],
    tools:       Vec<Tool>,
    tool_choice: ToolChoice,
    /// Zero temperature for deterministic Lua output.
    temperature: f32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Message {
    pub role:    String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Populated only for assistant messages that made tool calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallMsg>>,
    /// Populated only for tool-result messages sent back to the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Serialize)]
struct Tool {
    #[serde(rename = "type")]
    tool_type: &'static str,
    function:  FunctionDef,
}

#[derive(Serialize)]
struct FunctionDef {
    name:        String,
    description: String,
    parameters:  serde_json::Value,
}

/// Force the model to always call our script execution function.
#[derive(Serialize)]
struct ToolChoice {
    #[serde(rename = "type")]
    choice_type: &'static str,
    function:    FunctionName,
}

#[derive(Serialize)]
struct FunctionName {
    name: String,
}

// ── Response types ────────────────────────────────────────────────────────────

#[derive(Deserialize, Debug)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize, Debug)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Deserialize, Debug)]
struct ResponseMessage {
    tool_calls: Option<Vec<ToolCallMsg>>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ToolCallMsg {
    pub id:       String,
    pub function: FunctionCall,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FunctionCall {
    pub name:      String,
    /// Raw JSON string — must be parsed to extract `body`.
    pub arguments: String,
}

// ── Client ────────────────────────────────────────────────────────────────────

pub struct GroqClient {
    http:    reqwest::blocking::Client,
    api_key: String,
    model:   String,
}

impl GroqClient {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            http: reqwest::blocking::Client::new(),
            api_key,
            model,
        }
    }

    /// Call the Groq API with the script execution function forced.
    ///
    /// Returns `(script_body, call_id)` where:
    /// - `script_body` is the script the model wrote
    /// - `call_id` is the tool-call ID (needed for corrective feedback turns)
    pub fn get_script(
        &self,
        messages:         &[Message],
        function_name:    &str,
        tool_description: &str,
        body_description: &str,
    ) -> Result<(String, String), String> {
        let execute_tool = Tool {
            tool_type: "function",
            function: FunctionDef {
                name:        function_name.to_string(),
                description: tool_description.to_string(),
                parameters:  serde_json::json!({
                    "type": "object",
                    "properties": {
                        "body": {
                            "type": "string",
                            "description": body_description
                        }
                    },
                    "required": ["body"]
                }),
            },
        };

        let req = ChatRequest {
            model:       &self.model,
            messages,
            tools:       vec![execute_tool],
            tool_choice: ToolChoice {
                choice_type: "function",
                function:    FunctionName { name: function_name.to_string() },
            },
            temperature: 0.0,
        };

        let response = self
            .http
            .post(format!("{}/chat/completions", GROQ_BASE_URL))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&req)
            .send()
            .map_err(|e| format!("HTTP request failed: {e}"))?;

        let status = response.status();
        let body   = response.text().map_err(|e| format!("Failed to read body: {e}"))?;

        if !status.is_success() {
            return Err(format!("Groq API error {status}: {body}"));
        }

        let parsed: ChatResponse =
            serde_json::from_str(&body).map_err(|e| format!("JSON parse error: {e}"))?;

        // Extract the first tool call from the response
        let tool_call = parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.tool_calls)
            .and_then(|tcs| tcs.into_iter().next())
            .ok_or_else(|| "Model returned no tool call — check tool_choice config".to_string())?;

        // Parse the arguments JSON to extract `body`
        let args: serde_json::Value = serde_json::from_str(&tool_call.function.arguments)
            .map_err(|e| format!("Failed to parse tool arguments: {e}"))?;

        let script_body = args["body"]
            .as_str()
            .ok_or_else(|| format!("{function_name} missing required `body` field"))?
            .to_string();

        Ok((script_body, tool_call.id))
    }
}
