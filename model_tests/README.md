# Auwgent Model Integration Tests

This workspace member (`model_tests`) is a dedicated integration testing suite designed to validate the real-world performance of LLMs against the Auwgent Luau sandbox engine.

Instead of unit-testing the Rust engine with hardcoded strings, this suite makes live API calls to LLMs (via Groq), forces them to write Lua code to solve specific scenarios, executes that code in the sandbox, and validates the architectural outcome (e.g., verifying parallel batching, data dependencies, and correct data output).

## Setup

1. Copy the environment template in the workspace root:
   ```bash
   cp ../.env.example ../.env
   ```
2. Add your Groq API key to the new `.env` file. (Get one for free at [console.groq.com](https://console.groq.com)).

## Running Tests

Run the suite from the workspace root (or inside this directory):

```bash
# Run all scenarios against the default model (openai/gpt-oss-120b)
cargo run --release -p model_tests

# See the exact Lua code the model generated for each scenario
cargo run --release -p model_tests -- --verbose

# Run a specific scenario against a different model
cargo run --release -p model_tests -- --model llama-3.3-70b-versatile --scenario parallel

# Measure the "Orphan Rate" by running the same task 10 times
cargo run --release -p model_tests -- --scenario orphan --rounds 10
```

> **Note:** We strongly recommend running the tests in `--release` mode. Debug builds of the heavy asynchronous dependencies (`reqwest`, `tokio`) combined with `mlua` can hit the MSVC Windows PDB file size limit during the linking phase.

## Scenarios

The suite evaluates models across five key architectural capabilities:

| Scenario | Objective | Pass Criteria |
| :--- | :--- | :--- |
| **`basic`** | Standard function calling. | Model successfully maps task requirements to a single tool call using `await_all()`. |
| **`parallel`** | Parallel Tool Yield. | Model correctly batches two independent tool calls into a single `await_all()` tuple, proving Auwgent's N+1 prevention works. |
| **`chained`** | Data Dependencies. | Model correctly performs a sequential yield, using the output of round 1 as the input for round 2. |
| **`data`** | Pure Computation. | Model leverages native Lua to manipulate an injected global array without calling tools, proving the sandbox can execute logic safely. |
| **`orphan`** | Contract Compliance. | Stress-test to measure how often an LLM constructs a tool call but forgets to execute it via `await_all()`. |

## Interpreting Outcomes

The validator assigns one of three outcomes to every run:
* 🟢 **PASS:** The model wrote syntactically valid Lua, obeyed the `await_all` contract, and structurally solved the scenario.
* 🟡 **WARN:** The scenario executed successfully and the structure was sound, but the stdout/return payload slightly deviated from expected heuristics (frequent with open-source models formatting answers differently).
* 🔴 **FAIL:** The model failed to use `await_all` (resulting in Orphans), wrote invalid Lua syntax, or hallucinated tools.
