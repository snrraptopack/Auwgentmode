/// QuickJS sandbox integration tests.
///
/// These tests mirror the behavioral guarantees of the Luau test suite in
/// `src/tests.rs`, adapted for JavaScript semantics. Every test validates
/// a specific property of `QuickJsSandbox` in isolation so regressions are
/// easy to diagnose.
use auwgent_mode::{ExecutionResult, QuickJsSandbox, ToolDefinition, ToolResult};

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn weather_tool() -> ToolDefinition {
    ToolDefinition {
        name: "get_weather".into(),
        description: "Get the weather for a city".into(),
        has_args: true,
        arg_schema: Some("{ city: string }".into()),
    }
}

fn noop_tool() -> ToolDefinition {
    ToolDefinition {
        name: "ping".into(),
        description: "Ping with no args".into(),
        has_args: false,
        arg_schema: None,
    }
}

fn time_tool() -> ToolDefinition {
    ToolDefinition {
        name: "get_time".into(),
        description: "Get current time".into(),
        has_args: false,
        arg_schema: None,
    }
}

// ─── Basic execution ──────────────────────────────────────────────────────────

#[test]
fn test_js_basic_execution_no_tools() {
    let mut sb = QuickJsSandbox::new().unwrap();
    let result = sb.execute("// no tools needed").unwrap();
    assert!(matches!(result, ExecutionResult::Finished { .. }));
}

#[test]
fn test_js_console_log_captured() {
    let mut sb = QuickJsSandbox::new().unwrap();
    sb.execute(r#"console.log("hello from JS");"#).unwrap();
    assert_eq!(sb.get_console_output().trim(), "hello from JS");
}

#[test]
fn test_js_console_log_multiple_args() {
    let mut sb = QuickJsSandbox::new().unwrap();
    sb.execute(r#"console.log("a", 1, true);"#).unwrap();
    assert_eq!(sb.get_console_output().trim(), "a 1 true");
}

// ─── Tool yielding ────────────────────────────────────────────────────────────

#[test]
fn test_js_single_tool_yield() {
    let mut sb = QuickJsSandbox::new().unwrap();
    sb.register_tools(&[weather_tool()]).unwrap();

    let result = sb.execute(r#"
        const w = await get_weather({ city: "Lagos" });
        console.log(w.temp);
    "#).unwrap();

    let tools = match result {
        ExecutionResult::YieldedForTools { tools } => tools,
        other => panic!("expected YieldedForTools, got {other:?}"),
    };

    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].tool_name, "get_weather");
    assert_eq!(tools[0].payload["city"], "Lagos");
}

#[test]
fn test_js_tool_result_injected() {
    let mut sb = QuickJsSandbox::new().unwrap();
    sb.register_tools(&[weather_tool()]).unwrap();

    sb.execute(r#"
        const w = await get_weather({ city: "Abuja" });
        console.log(w.temp);
    "#).unwrap();

    let result = sb
        .resume_with_results(vec![ToolResult::Ok(serde_json::json!({ "temp": 32 }))])
        .unwrap();

    assert!(matches!(result, ExecutionResult::Finished { .. }));
    assert_eq!(sb.get_console_output().trim(), "32");
}

#[test]
fn test_js_no_args_tool() {
    let mut sb = QuickJsSandbox::new().unwrap();
    sb.register_tools(&[noop_tool()]).unwrap();

    let result = sb.execute("const r = await ping(); console.log(r);").unwrap();

    let tools = match result {
        ExecutionResult::YieldedForTools { tools } => tools,
        other => panic!("expected YieldedForTools, got {other:?}"),
    };
    assert_eq!(tools[0].tool_name, "ping");
}

// ─── Multi-round execution ────────────────────────────────────────────────────

#[test]
fn test_js_sequential_tool_calls() {
    let mut sb = QuickJsSandbox::new().unwrap();
    sb.register_tools(&[weather_tool(), time_tool()]).unwrap();

    // First round: get_weather
    let r1 = sb.execute(r#"
        const w = await get_weather({ city: "Kano" });
        const t = await get_time();
        console.log(w.temp + "°C at " + t.time);
    "#).unwrap();

    assert!(matches!(r1, ExecutionResult::YieldedForTools { .. }));
    let r2 = sb.resume_with_results(vec![
        ToolResult::Ok(serde_json::json!({ "temp": 40 })),
    ]).unwrap();

    assert!(matches!(r2, ExecutionResult::YieldedForTools { .. }));
    let r3 = sb.resume_with_results(vec![
        ToolResult::Ok(serde_json::json!({ "time": "12:00" })),
    ]).unwrap();

    assert!(matches!(r3, ExecutionResult::Finished { .. }));
    assert_eq!(sb.get_console_output().trim(), "40°C at 12:00");
}

#[test]
fn test_js_parallel_tool_calls() {
    let mut sb = QuickJsSandbox::new().unwrap();
    sb.register_tools(&[weather_tool(), time_tool()]).unwrap();

    // Promise.all-style parallel: both tools yielded in one batch.
    let r1 = sb.execute(r#"
        const [w, t] = await Promise.all([
            get_weather({ city: "Port Harcourt" }),
            get_time()
        ]);
        console.log(w.temp + " " + t.time);
    "#).unwrap();

    let tools = match r1 {
        ExecutionResult::YieldedForTools { tools } => tools,
        other => panic!("expected YieldedForTools, got {other:?}"),
    };
    assert_eq!(tools.len(), 2);

    let r2 = sb.resume_with_results(vec![
        ToolResult::Ok(serde_json::json!({ "temp": 27 })),
        ToolResult::Ok(serde_json::json!({ "time": "09:00" })),
    ]).unwrap();

    assert!(matches!(r2, ExecutionResult::Finished { .. }));
    assert_eq!(sb.get_console_output().trim(), "27 09:00");
}

// ─── Error handling ───────────────────────────────────────────────────────────

#[test]
fn test_js_tool_error_sentinel() {
    let mut sb = QuickJsSandbox::new().unwrap();
    sb.register_tools(&[weather_tool()]).unwrap();

    sb.execute(r#"
        const w = await get_weather({ city: "Unknown" });
        if (w.__error) {
            console.log("error: " + w.message);
        }
    "#).unwrap();

    let result = sb.resume_with_results(vec![
        ToolResult::Err("City not found".into()),
    ]).unwrap();

    assert!(matches!(result, ExecutionResult::Finished { .. }));
    assert_eq!(sb.get_console_output().trim(), "error: City not found");
}

#[test]
fn test_js_syntax_error_caught() {
    let mut sb = QuickJsSandbox::new().unwrap();
    let result = sb.execute("this is not valid javascript !!!@@@");
    // Should return an error, not panic
    assert!(
        result.is_err()
            || matches!(result.unwrap(), ExecutionResult::Error(_)),
        "Expected error for invalid JS"
    );
}

// ─── Globals injection ────────────────────────────────────────────────────────

#[test]
fn test_js_inject_globals() {
    let mut sb = QuickJsSandbox::new().unwrap();
    sb.inject_globals(serde_json::json!({ "CITY": "Ibadan", "LIMIT": 5 })).unwrap();
    sb.execute(r#"console.log(CITY + " " + LIMIT);"#).unwrap();
    assert_eq!(sb.get_console_output().trim(), "Ibadan 5");
}

// ─── Infinite-loop protection ─────────────────────────────────────────────────

#[test]
fn test_js_infinite_loop_protection() {
    let mut sb = QuickJsSandbox::new().unwrap();
    let result = sb.execute("while(true) {}");
    // Must return Error — not hang.
    assert!(
        result.is_err()
            || matches!(result.unwrap(), ExecutionResult::Error(_)),
        "Expected infinite loop to be interrupted"
    );
}

// ─── Library loading ──────────────────────────────────────────────────────────

#[test]
fn test_js_load_library() {
    let mut sb = QuickJsSandbox::new().unwrap();
    sb.load_library("function greet(name) { return 'Hello, ' + name; }").unwrap();
    sb.execute(r#"console.log(greet("Auwgent"));"#).unwrap();
    assert_eq!(sb.get_console_output().trim(), "Hello, Auwgent");
}

// ─── Snapshot / restore ───────────────────────────────────────────────────────

#[test]
fn test_js_snapshot_restore_fast_forwards() {
    let mut sb = QuickJsSandbox::new().unwrap();
    sb.register_tools(&[weather_tool()]).unwrap();

    // Execute to first yield.
    sb.execute(r#"
        const w = await get_weather({ city: "Enugu" });
        console.log(w.temp);
    "#).unwrap();

    let snap = sb.snapshot().unwrap();

    // Resolve the first yield.
    sb.resume_with_results(vec![
        ToolResult::Ok(serde_json::json!({ "temp": 28 })),
    ]).unwrap();

    // Restore and re-execute from snapshot.
    let (mut restored, status) = QuickJsSandbox::from_snapshot(snap).unwrap();

    // The snapshot had 0 completed rounds, so restore should be at first yield.
    assert!(matches!(status, ExecutionResult::YieldedForTools { .. }));

    // Resolve again to completion.
    let final_result = restored.resume_with_results(vec![
        ToolResult::Ok(serde_json::json!({ "temp": 28 })),
    ]).unwrap();
    assert!(matches!(final_result, ExecutionResult::Finished { .. }));
    assert_eq!(restored.get_console_output().trim(), "28");
}

#[test]
fn test_js_snapshot_with_completed_rounds_fast_forwards() {
    let mut sb = QuickJsSandbox::new().unwrap();
    sb.register_tools(&[weather_tool(), time_tool()]).unwrap();

    // Execute through round 1.
    sb.execute(r#"
        const w = await get_weather({ city: "Owerri" });
        const t = await get_time();
        console.log(w.temp + " " + t.time);
    "#).unwrap();

    sb.resume_with_results(vec![
        ToolResult::Ok(serde_json::json!({ "temp": 30 })),
    ]).unwrap();

    // Snapshot after round 1 is completed (we're now at round 2 yield).
    let snap = sb.snapshot().unwrap();
    assert_eq!(snap.completed_tool_results.len(), 1);

    // Restore — should fast-forward through round 1 and land at round 2 yield.
    let (_restored, status) = QuickJsSandbox::from_snapshot(snap).unwrap();
    assert!(matches!(status, ExecutionResult::YieldedForTools { .. }));
}

#[test]
fn test_js_snapshot_library_survives_restore() {
    let mut sb = QuickJsSandbox::new().unwrap();
    sb.load_library("function square(x) { return x * x; }").unwrap();
    sb.register_tools(&[noop_tool()]).unwrap();

    sb.execute(r#"
        await ping();
        console.log(square(7));
    "#).unwrap();

    let snap = sb.snapshot().unwrap();

    let (mut restored, _) = QuickJsSandbox::from_snapshot(snap).unwrap();
    restored.resume_with_results(vec![ToolResult::Ok(serde_json::json!({}))]).unwrap();
    assert_eq!(restored.get_console_output().trim(), "49");
}

// ─── Tool prompt generation ───────────────────────────────────────────────────

#[test]
fn test_js_generate_tool_prompt() {
    let tools = vec![
        ToolDefinition {
            name: "search".into(),
            description: "Search the web".into(),
            has_args: true,
            arg_schema: Some("{ query: string }".into()),
        },
        noop_tool(),
    ];
    let prompt = QuickJsSandbox::generate_tool_prompt(&tools);
    assert!(prompt.contains("`search`"));
    assert!(prompt.contains("Search the web"));
    assert!(prompt.contains("{ query: string }"));
    assert!(prompt.contains("(no arguments)"));
}
