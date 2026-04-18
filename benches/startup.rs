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

// ─── New Benchmarks ───────────────────────────────────────────────────────────

/// Payload size scaling — measures `resume_with_json` cost as tool result
/// payload grows from small (~50 bytes) to medium (~1 KB) to large (~50 KB).
/// Identifies at what payload size JSON deserialization becomes the bottleneck.
fn bench_payload_size_scaling(c: &mut Criterion) {
    let tools = vec![make_tool("fetch_data", false)];
    let script = "local r = await_all(fetch_data()) return tostring(#r.items)";

    // Small: ~50 bytes — a handful of scalar fields
    let small_payload = serde_json::json!({ "items": ["a", "b", "c"], "count": 3 });

    // Medium: ~1 KB — ~50 key-value pairs
    let medium_items: Vec<serde_json::Value> = (0..50)
        .map(|i| serde_json::json!({ "id": i, "name": format!("item_{:04}", i), "score": i * 10 }))
        .collect();
    let medium_payload = serde_json::json!({ "items": medium_items, "count": 50 });

    // Large: ~50 KB — 2,500 item array, simulating a database result set
    let large_items: Vec<serde_json::Value> = (0..2500)
        .map(|i| serde_json::json!({ "id": i, "value": format!("data_{}", i) }))
        .collect();
    let large_payload = serde_json::json!({ "items": large_items, "count": 2500 });

    let mut group = c.benchmark_group("payload_size");

    group.bench_function("small_50b", |b| {
        b.iter(|| {
            let mut engine = AuwgentSandbox::new().unwrap();
            engine.register_tools(&tools).unwrap();
            match engine.execute(script).unwrap() {
                ExecutionResult::YieldedForTools { .. } => engine
                    .resume_with_json(vec![small_payload.clone()])
                    .unwrap(),
                other => other,
            }
        });
    });

    group.bench_function("medium_1kb", |b| {
        b.iter(|| {
            let mut engine = AuwgentSandbox::new().unwrap();
            engine.register_tools(&tools).unwrap();
            match engine.execute(script).unwrap() {
                ExecutionResult::YieldedForTools { .. } => engine
                    .resume_with_json(vec![medium_payload.clone()])
                    .unwrap(),
                other => other,
            }
        });
    });

    group.bench_function("large_50kb", |b| {
        b.iter(|| {
            let mut engine = AuwgentSandbox::new().unwrap();
            engine.register_tools(&tools).unwrap();
            match engine.execute(script).unwrap() {
                ExecutionResult::YieldedForTools { .. } => engine
                    .resume_with_json(vec![large_payload.clone()])
                    .unwrap(),
                other => other,
            }
        });
    });

    group.finish();
}

/// N-instance memory pressure — measures the cost of creating and holding
/// N `AuwgentSandbox` instances alive simultaneously.
///
/// Each sandbox has a 20MB heap *cap*. This measures real allocator pressure,
/// not theoretical worst-case. In practice each idle sandbox with no active
/// script uses only a few KB of actual memory.
///
/// Note: `AuwgentSandbox` is `!Send` (mlua Lua is not thread-safe by default),
/// so concurrency here is measured as serial creation overhead, not parallel execution.
fn bench_n_instance_pressure(c: &mut Criterion) {
    let mut group = c.benchmark_group("instance_pressure");

    for &n in &[10usize, 50, 100] {
        group.bench_function(format!("{}_instances_serial", n), |b| {
            b.iter(|| {
                // Create N sandboxes and keep them alive simultaneously to
                // measure peak allocator pressure per-sandbox.
                let engines: Vec<AuwgentSandbox> = (0..n)
                    .map(|_| AuwgentSandbox::new().unwrap())
                    .collect();
                // Return the vec so the compiler doesn't optimize the allocation away
                engines.len()
            });
        });
    }

    group.finish();
}

/// Script complexity scaling — measures parse + bytecode compile time as the
/// LLM-generated script grows in size: 10 lines, 100 lines, 500 lines.
/// Relevant because LLMs sometimes emit verbose boilerplate on complex tasks.
fn bench_script_complexity_scaling(c: &mut Criterion) {
    /// Generate a Lua script that simulates verbose LLM output at `lines` scale.
    ///
    /// For small counts (≤ 100), use separate local declarations — typical LLM style.
    /// For larger counts, switch to a table accumulator to stay within Luau's
    /// 200-register local variable limit while still measuring parse/compile cost.
    fn make_script(lines: usize) -> String {
        if lines <= 100 {
            // Direct locals — typical for short LLM scripts
            let locals: String = (0..lines)
                .map(|i| format!("local v{i} = {i} * 2 + math.sqrt({i})\n"))
                .collect();
            format!("{}\nreturn tostring(v{})", locals, lines - 1)
        } else {
            // Table accumulator — avoids the 200-register limit while still
            // exercising the parser and bytecode compiler on large inputs.
            let entries: String = (0..lines)
                .map(|i| format!("results[{}] = {} * 2 + math.sqrt({})\n", i + 1, i, i))
                .collect();
            format!(
                "local results = {{}}\n{}\nreturn tostring(results[{}])",
                entries,
                lines
            )
        }
    }

    let script_10  = make_script(10);
    let script_100 = make_script(100);
    let script_500 = make_script(500);

    let mut group = c.benchmark_group("script_complexity");

    group.bench_function("10_lines", |b| {
        b.iter(|| {
            let mut engine = AuwgentSandbox::new().unwrap();
            engine.execute(&script_10).unwrap()
        });
    });

    group.bench_function("100_lines", |b| {
        b.iter(|| {
            let mut engine = AuwgentSandbox::new().unwrap();
            engine.execute(&script_100).unwrap()
        });
    });

    group.bench_function("500_lines", |b| {
        b.iter(|| {
            let mut engine = AuwgentSandbox::new().unwrap();
            engine.execute(&script_500).unwrap()
        });
    });

    group.finish();
}

/// Tool registration scaling — measures `register_tools()` cost as the
/// tool registry grows: 5, 20, 50, 100 tools.
/// Relevant for large agent deployments with domain-wide intent registries.
fn bench_tool_registration_scaling(c: &mut Criterion) {
    fn make_tools(n: usize) -> Vec<ToolDefinition> {
        (0..n)
            .map(|i| ToolDefinition {
                name: format!("tool_{:04}", i),
                description: format!("Benchmark tool #{}", i),
                has_args: i % 2 == 0, // mix of arg/no-arg stubs
                arg_schema: None,
            })
            .collect()
    }

    let tools_5   = make_tools(5);
    let tools_20  = make_tools(20);
    let tools_50  = make_tools(50);
    let tools_100 = make_tools(100);

    let mut group = c.benchmark_group("tool_registration");

    group.bench_function("5_tools", |b| {
        b.iter(|| {
            let mut engine = AuwgentSandbox::new().unwrap();
            engine.register_tools(&tools_5).unwrap()
        });
    });

    group.bench_function("20_tools", |b| {
        b.iter(|| {
            let mut engine = AuwgentSandbox::new().unwrap();
            engine.register_tools(&tools_20).unwrap()
        });
    });

    group.bench_function("50_tools", |b| {
        b.iter(|| {
            let mut engine = AuwgentSandbox::new().unwrap();
            engine.register_tools(&tools_50).unwrap()
        });
    });

    group.bench_function("100_tools", |b| {
        b.iter(|| {
            let mut engine = AuwgentSandbox::new().unwrap();
            engine.register_tools(&tools_100).unwrap()
        });
    });

    group.finish();
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
    bench_payload_size_scaling,
    bench_n_instance_pressure,
    bench_script_complexity_scaling,
    bench_tool_registration_scaling,
);
criterion_main!(benches);
