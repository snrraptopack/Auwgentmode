/// Colored terminal output for test results.
use colored::Colorize;

use crate::scenarios::ScenarioOutcome;

pub fn print_header(model: &str, engine: &str) {
    println!();
    println!("{}", "═".repeat(62).cyan());
    println!(
        "  {}  [{} | {}]",
        "Auwgent Model Tests".bold(),
        model.yellow(),
        engine.yellow()
    );
    println!("{}", "═".repeat(62).cyan());
}

pub fn print_result(
    name:        &str,
    outcome:     &ScenarioOutcome,
    tool_rounds: usize,
    orphans:     usize,
    duration_ms: u128,
    language:    &str,
    verbose_script: Option<&str>,
) {
    let (badge, detail_str) = match outcome {
        ScenarioOutcome::Pass(msg) => (" PASS ".on_green().bold(),  msg.green().to_string()),
        ScenarioOutcome::Fail(msg) => (" FAIL ".on_red().bold(),    msg.red().to_string()),
        ScenarioOutcome::Warn(msg) => (" WARN ".on_yellow().bold(), msg.yellow().to_string()),
    };

    let orphan_tag = if orphans > 0 {
        format!(" · {} orphan(s)", orphans).red().to_string()
    } else {
        String::new()
    };

    println!(
        "  {}  {:<12}  {} rounds{}· {}ms",
        badge,
        name.bold(),
        tool_rounds,
        orphan_tag,
        duration_ms,
    );
    println!("              {}", detail_str);

    // In verbose mode, print the script the model wrote underneath.
    if let Some(script) = verbose_script {
        println!();
        println!(
            "{}",
            format!("  -- Generated {language} --------------------------------").dimmed()
        );
        for line in script.lines() {
            println!("  {}", line.dimmed());
        }
        println!("{}", "  ────────────────────────────────────────────────────".dimmed());
        println!();
    }
}

pub fn print_summary(pass: usize, fail: usize, warn: usize) {
    println!("{}", "═".repeat(62).cyan());
    let p = format!("{pass} passed").green().bold();
    let f = if fail > 0 { format!(" · {fail} failed").red().bold().to_string() } else { String::new() };
    let w = if warn > 0 { format!(" · {warn} warned").yellow().bold().to_string() } else { String::new() };
    println!("  {p}{f}{w}");
    println!("{}", "═".repeat(62).cyan());
    println!();
}
