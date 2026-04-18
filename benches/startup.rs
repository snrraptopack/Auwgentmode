/// Startup and execution benchmarks for AuwgentSandbox.
///
/// Run with:  cargo bench
/// HTML reports saved to: target/criterion/
use auwgent_mode::{AuwgentSandbox, ExecutionResult, SandboxSnapshot, ToolDefinition, ToolResult};
use criterion::{Criterion, criterion_group, criterion_main};

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn make_tool(name: &str, has_args: bool) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: format!("Benchmark tool: {}", name),
        has_args,
        arg_schema: None,
    }
}

/// Build a snapshot with N completed tool rounds for restore benchmarks.
fn build_snapshot_with_rounds(n: usize) -> SandboxSnapshot {
    let tools = vec![make_tool("step", false)];

    // Build a script that yields N times sequentially
    let yields: String = (0..n)
        .map(|i| format!("local _r{i} = await_all(step())\n"))
        .collect();
    let script = format!("{}\nreturn \"done\"", yields);

    let mut engine = AuwgentSandbox::new().unwrap();
    engine.register_tools(&tools).unwrap();

    let mut status = engine.execute(&script).unwrap();
    for _ in 0..n {
        match status {
            ExecutionResult::YieldedForTools { .. } => {
                status = engine
                    .resume_with_json(vec![serde_json::json!({ "ok": true })])
                    .unwrap();
            }
            _ => break,
        }
    }
    engine.snapshot().unwrap()
}

// ─── Benchmarks ──────────────────────────────────────────────────────────────

/// How long does a cold Lua VM take to boot?
/// This measures AuwgentSandbox::new() in isolation — no tools, no script.
fn bench_cold_startup(c: &mut Criterion) {
    c.bench_function("cold_vm_startup", |b| {
        b.iter(|| {
            AuwgentSandbox::new().unwrap()
        });
    });
}

/// How long from boot to first Lua instruction executing?
/// Measures new() + register_tools() + execute() on a trivial no-yield script.
fn bench_boot_to_first_execute(c: &mut Criterion) {
    let tools = vec![make_tool("get_data", true)];

    c.bench_function("boot_to_first_execute", |b| {
        b.iter(|| {
            let mut engine = AuwgentSandbox::new().unwrap();
            engine.register_tools(&tools).unwrap();
            engine.execute("return 42").unwrap()
        });
    });
}

/// Full round-trip: boot → register tools → execute → yield → resume → finish.
/// This is the core hot path for a single-tool agentic request.
fn bench_single_tool_roundtrip(c: &mut Criterion) {
    let tools = vec![make_tool("get_weather", true)];

    let script = r#"
        local w = await_all(get_weather({ location = "Lagos" }))
        return w.condition
    "#;

    c.bench_function("single_tool_roundtrip", |b| {
        b.iter(|| {
            let mut engine = AuwgentSandbox::new().unwrap();
            engine.register_tools(&tools).unwrap();

            match engine.execute(script).unwrap() {
                ExecutionResult::YieldedForTools { .. } => {
                    engine
                        .resume_with_json(vec![serde_json::json!({ "condition": "Sunny" })])
                        .unwrap()
                }
                other => other,
            }
        });
    });
}

/// Parallel yield: two tools in one await_all, one round-trip.
fn bench_parallel_two_tools(c: &mut Criterion) {
    let tools = vec![
        make_tool("get_weather", true),
        make_tool("get_stocks", true),
    ];

    let script = r#"
        local w, s = await_all(
            get_weather({ location = "NY" }),
            get_stocks({ ticker = "AAPL" })
        )
        return w.temp .. s.price
    "#;

    c.bench_function("parallel_two_tools_roundtrip", |b| {
        b.iter(|| {
            let mut engine = AuwgentSandbox::new().unwrap();
            engine.register_tools(&tools).unwrap();

            match engine.execute(script).unwrap() {
                ExecutionResult::YieldedForTools { .. } => engine
                    .resume_with_json(vec![
                        serde_json::json!({ "temp": "32C" }),
                        serde_json::json!({ "price": "189" }),
                    ])
                    .unwrap(),
                other => other,
            }
        });
    });
}

/// How long does resume_with_results take vs resume_with_json?
/// Should be nearly identical since results are materialized to JSON before inject.
fn bench_resume_with_results_vs_json(c: &mut Criterion) {
    let tools = vec![make_tool("step", false)];
    let script = "local r = await_all(step()) return r.val";

    let mut group = c.benchmark_group("resume_variant");

    group.bench_function("resume_with_json", |b| {
        b.iter(|| {
            let mut engine = AuwgentSandbox::new().unwrap();
            engine.register_tools(&tools).unwrap();
            match engine.execute(script).unwrap() {
                ExecutionResult::YieldedForTools { .. } => engine
                    .resume_with_json(vec![serde_json::json!({ "val": 1 })])
                    .unwrap(),
                other => other,
            }
        });
    });

    group.bench_function("resume_with_results_ok", |b| {
        b.iter(|| {
            let mut engine = AuwgentSandbox::new().unwrap();
            engine.register_tools(&tools).unwrap();
            match engine.execute(script).unwrap() {
                ExecutionResult::YieldedForTools { .. } => engine
                    .resume_with_results(vec![ToolResult::Ok(serde_json::json!({ "val": 1 }))])
                    .unwrap(),
                other => other,
            }
        });
    });

    group.bench_function("resume_with_results_err", |b| {
        b.iter(|| {
            let mut engine = AuwgentSandbox::new().unwrap();
            engine.register_tools(&tools).unwrap();
            match engine.execute(script).unwrap() {
                ExecutionResult::YieldedForTools { .. } => engine
                    .resume_with_results(vec![ToolResult::Err("timeout".into())])
                    .unwrap(),
                other => other,
            }
        });
    });

    group.finish();
}

/// Snapshot restore time with varying numbers of completed rounds.
/// Shows how fast-forward cost scales with session depth.
fn bench_snapshot_restore(c: &mut Criterion) {
    let mut group = c.benchmark_group("snapshot_restore");

    for &rounds in &[1usize, 5, 10, 20] {
        let snap = build_snapshot_with_rounds(rounds);

        group.bench_function(format!("{}_rounds", rounds), |b| {
            b.iter(|| {
                AuwgentSandbox::from_snapshot(snap.clone()).unwrap()
            });
        });
    }

    group.finish();
}

/// Pure Lua computation speed — no tools, no yields.
/// Measures the cost of the Lua VM executing a tight loop.
fn bench_pure_lua_computation(c: &mut Criterion) {
    c.bench_function("pure_lua_1000_iterations", |b| {
        b.iter(|| {
            let mut engine = AuwgentSandbox::new().unwrap();
            engine
                .execute(
                    r#"
                local sum = 0
                for i = 1, 1000 do
                    sum = sum + i
                end
                return tostring(sum)
            "#,
                )
                .unwrap()
        });
    });
}

// ─── Registration ────────────────────────────────────────────────────────────

criterion_group!(
    benches,
    bench_cold_startup,
    bench_boot_to_first_execute,
    bench_single_tool_roundtrip,
    bench_parallel_two_tools,
    bench_resume_with_results_vs_json,
    bench_snapshot_restore,
    bench_pure_lua_computation,
);
criterion_main!(benches);
