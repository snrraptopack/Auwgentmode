/// CLI entrypoint for Auwgent model integration tests.
///
/// Loads `GROQ_API_KEY` from `Auwgentmode/.env`, builds a Groq client,
/// then runs each scenario through the `AuwgentAgent` loop against real LLM output.
///
/// Usage:
///   cargo run -p model_tests                              # all scenarios, default model
///   cargo run -p model_tests -- --model openai/gpt-oss-120b
///   cargo run -p model_tests -- --scenario parallel --verbose
///   cargo run -p model_tests -- --scenario orphan --rounds 5
use clap::Parser;

use crate::agent::AuwgentAgent;
use crate::client::GroqClient;
use crate::reporter::{print_header, print_result, print_summary};
use crate::scenarios::{all_scenarios, scenario_by_name, ScenarioOutcome};

mod agent;
mod client;
mod reporter;
mod scenarios;

// ── CLI args ──────────────────────────────────────────────────────────────────

/// Default Groq model for the tests.
const DEFAULT_MODEL: &str = "openai/gpt-oss-120b";

#[derive(Parser)]
#[command(
    name    = "model_tests",
    about   = "Auwgent Mode — LLM integration tests via Groq",
    version
)]
struct Args {
    /// Groq model ID to use (e.g. llama-3.3-70b-versatile)
    #[arg(long, short, default_value = DEFAULT_MODEL)]
    model: String,

    /// Run only one scenario: basic | parallel | chained | data | orphan
    #[arg(long, short)]
    scenario: Option<String>,

    /// Print the raw Lua script the model generated for each scenario
    #[arg(long, short)]
    verbose: bool,

    /// How many times to run each scenario (useful for orphan rate measurement)
    #[arg(long, short, default_value = "1")]
    rounds: usize,
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    // Load GROQ_API_KEY from Auwgentmode/.env (silently ok if file absent)
    dotenvy::dotenv().ok();

    let args = Args::parse();

    let api_key = std::env::var("GROQ_API_KEY").unwrap_or_else(|_| {
        eprintln!(
            "Error: GROQ_API_KEY not set.\n\
             Copy Auwgentmode/.env.example → Auwgentmode/.env and fill in your key.\n\
             Get a free key at https://console.groq.com"
        );
        std::process::exit(1);
    });

    let client = GroqClient::new(api_key, args.model.clone());

    print_header(&args.model);

    // ── Resolve scenario list ─────────────────────────────────────────────────
    let scenarios = if let Some(name) = &args.scenario {
        match scenario_by_name(name) {
            Some(s) => vec![s],
            None => {
                // Bind to a named variable so the temporaries live long enough
                // for the collected &str slices (which borrow from the boxes).
                let available = all_scenarios();
                let names: Vec<&str> = available.iter().map(|s| s.name()).collect();
                eprintln!(
                    "Unknown scenario '{}'. Available: {}",
                    name,
                    names.join(", ")
                );
                std::process::exit(1);
            }
        }
    } else {
        all_scenarios()
    };

    let runs_per_scenario = args.rounds.max(1);
    let mut pass = 0usize;
    let mut fail = 0usize;
    let mut warn = 0usize;

    // ── Run each scenario ─────────────────────────────────────────────────────
    for scenario in &scenarios {
        let mut orphan_count = 0usize;
        let mut last_run     = None;

        for _ in 0..runs_per_scenario {
            let agent = AuwgentAgent {
                client:  &client,
                tools:   scenario.tools(),
                globals: scenario.globals(),
            };

            let run = agent.run(scenario.task(), &|name, payload| {
                scenario.dispatch(name, payload)
            });

            if !run.orphaned_calls.is_empty() {
                orphan_count += 1;
            }
            last_run = Some(run);
        }

        let run = last_run.expect("at least one run must have completed");

        // ── Determine outcome ─────────────────────────────────────────────────
        // For the orphan scenario with multiple rounds, override the single-run
        // verdict with an aggregate orphan-rate assessment.
        let outcome = if scenario.name() == "orphan" && runs_per_scenario > 1 {
            let pct = (orphan_count * 100) / runs_per_scenario;
            if orphan_count == 0 {
                ScenarioOutcome::Pass(format!(
                    "0/{runs} runs had orphans (0% orphan rate)",
                    runs = runs_per_scenario
                ))
            } else if orphan_count == runs_per_scenario {
                ScenarioOutcome::Fail(format!(
                    "{orphan_count}/{runs} runs had orphans ({pct}% orphan rate)",
                    runs = runs_per_scenario
                ))
            } else {
                ScenarioOutcome::Warn(format!(
                    "{orphan_count}/{runs} runs had orphans ({pct}% orphan rate)",
                    runs = runs_per_scenario
                ))
            }
        } else {
            scenario.validate(&run)
        };

        match &outcome {
            ScenarioOutcome::Pass(_) => pass += 1,
            ScenarioOutcome::Fail(_) => fail += 1,
            ScenarioOutcome::Warn(_) => warn += 1,
        }

        // Verbose: show the Lua only on the last run
        let lua_display = if args.verbose { Some(run.lua_script.as_str()) } else { None };

        print_result(
            scenario.name(),
            &outcome,
            run.tool_rounds,
            run.orphaned_calls.len(),
            run.duration_ms,
            lua_display,
        );
    }

    print_summary(pass, fail, warn);

    // Exit with non-zero code if any scenario failed
    if fail > 0 {
        std::process::exit(1);
    }
}
