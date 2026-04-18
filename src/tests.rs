use super::*;

// ─── Helper ──────────────────────────────────────────────────────────────────

/// Convenience: build ToolDefinitions for tests quickly
fn make_tool(name: &str, description: &str, has_args: bool, schema: Option<&str>) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: description.to_string(),
        has_args,
        arg_schema: schema.map(|s| s.to_string()),
    }
}

/// Run a sandbox loop to completion, driving all tool yields automatically via
/// the supplied `dispatcher` closure. Returns the final ExecutionResult.
fn run_to_finish(
    engine: &mut AuwgentSandbox,
    script: &str,
    mut dispatcher: impl FnMut(&str, &serde_json::Value) -> serde_json::Value,
) -> ExecutionResult {
    let mut status = engine.execute(script).expect("execute() failed");
    loop {
        match status {
            ExecutionResult::YieldedForTools { tools } => {
                let responses: Vec<serde_json::Value> = tools
                    .iter()
                    .map(|t| dispatcher(&t.tool_name, &t.payload))
                    .collect();
                status = engine
                    .resume_with_json(responses)
                    .expect("resume_with_json() failed");
            }
            other => return other,
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// Basic: no tools, just print and return.
#[test]
fn test_basic_execution_no_tools() {
    let mut engine = AuwgentSandbox::new().unwrap();

    let script = r#"
        print("Hello from the sandbox!")
        return "done"
    "#;

    match engine.execute(script).unwrap() {
        ExecutionResult::Finished { ret_val, console_output } => {
            assert_eq!(ret_val.as_deref(), Some("done"));
            assert!(console_output.contains("Hello from the sandbox!"));
        }
        other => panic!("Expected Finished, got {:?}", other),
    }
}

/// A tool with no arguments is called correctly.
#[test]
fn test_tool_with_no_args() {
    let mut engine = AuwgentSandbox::new().unwrap();
    engine
        .register_tools(&[make_tool("ping", "Pings the server", false, None)])
        .unwrap();

    let script = r#"
        local result = await_all(ping())
        print("Ping response:", result.status)
        return "ok"
    "#;

    let result = run_to_finish(&mut engine, script, |name, _| {
        assert_eq!(name, "ping");
        serde_json::json!({ "status": "pong" })
    });

    match result {
        ExecutionResult::Finished { console_output, .. } => {
            assert!(console_output.contains("pong"));
        }
        other => panic!("Expected Finished, got {:?}", other),
    }
}

/// Multi-step: tool → Lua manipulation → second tool → finish.
#[test]
fn test_advanced_sandbox_loop() {
    let mut engine = AuwgentSandbox::new().unwrap();
    engine
        .register_tools(&[
            make_tool("fetch_user", "Fetch user record by ID", true, Some("{ id: string }")),
            make_tool("verify_status", "Verify status of a target", true, Some("{ target: string }")),
        ])
        .unwrap();

    let script = r#"
        print("Starting advanced multi-step task...")
        local user_data = await_all(fetch_user({ id = "user_777" }))
        print("Got user data!", user_data.name)
        local status_check = user_data.name .. "_check"
        local status_res = await_all(verify_status({ target = status_check }))
        print("Final result:", status_res.verified)
        return "SUCCESS"
    "#;

    let result = run_to_finish(&mut engine, script, |name, payload| match name {
        "fetch_user" => {
            assert_eq!(payload["id"], "user_777");
            serde_json::json!({ "name": "JohnDoe", "age": 30 })
        }
        "verify_status" => {
            assert_eq!(payload["target"], "JohnDoe_check");
            serde_json::json!({ "verified": true })
        }
        other => panic!("Unexpected tool: {}", other),
    });

    match result {
        ExecutionResult::Finished { ret_val, console_output } => {
            assert_eq!(ret_val.as_deref(), Some("SUCCESS"));
            assert!(console_output.contains("Got user data!\tJohnDoe"));
            assert!(console_output.contains("Final result:\ttrue"));
        }
        other => panic!("Expected Finished, got {:?}", other),
    }
}

/// Parallel tools: two tools yielded in one await_all call.
#[test]
fn test_parallel_tool_yield() {
    let mut engine = AuwgentSandbox::new().unwrap();
    engine
        .register_tools(&[
            make_tool("get_weather", "Fetch weather", true, Some("{ location: string }")),
            make_tool("get_stocks", "Fetch stock price", true, Some("{ ticker: string }")),
        ])
        .unwrap();

    let script = r#"
        local weather, stocks = await_all(
            get_weather({ location = "NY" }),
            get_stocks({ ticker = "AAPL" })
        )
        print("Weather:", weather.temp)
        print("Stock:", stocks.price)
        return "done"
    "#;

    let mut call_count = 0;
    let result = run_to_finish(&mut engine, script, |name, _| {
        call_count += 1;
        match name {
            "get_weather" => serde_json::json!({ "temp": "72F" }),
            "get_stocks" => serde_json::json!({ "price": "189.50" }),
            other => panic!("Unexpected: {}", other),
        }
    });

    // Both tools must be dispatched in a single yield round
    assert_eq!(call_count, 2);
    match result {
        ExecutionResult::Finished { console_output, .. } => {
            assert!(console_output.contains("72F"));
            assert!(console_output.contains("189.50"));
        }
        other => panic!("Expected Finished, got {:?}", other),
    }
}

/// Globals injection: AGENT_ID and WORKSPACE_PATH are accessible inside Lua.
#[test]
fn test_inject_globals() {
    let mut engine = AuwgentSandbox::new().unwrap();
    engine
        .inject_globals(serde_json::json!({
            "AGENT_ID": "agent_007",
            "WORKSPACE_PATH": "/app/project"
        }))
        .unwrap();

    let script = r#"
        print("Agent:", AGENT_ID)
        print("Workspace:", WORKSPACE_PATH)
        return AGENT_ID
    "#;

    match engine.execute(script).unwrap() {
        ExecutionResult::Finished { ret_val, console_output } => {
            assert_eq!(ret_val.as_deref(), Some("agent_007"));
            assert!(console_output.contains("agent_007"));
            assert!(console_output.contains("/app/project"));
        }
        other => panic!("Expected Finished, got {:?}", other),
    }
}

/// Security: the `os` library must NOT be accessible in the sandbox.
#[test]
fn test_sandbox_blocks_os_library() {
    let mut engine = AuwgentSandbox::new().unwrap();

    let script = r#"
        local result = os.time()
        return "should not reach here"
    "#;

    // os.time() must fail because `os` is not loaded
    match engine.execute(script) {
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("os") || msg.contains("nil") || msg.contains("attempt"),
                "Unexpected error: {}",
                msg
            );
        }
        Ok(ExecutionResult::Finished { .. }) => {
            panic!("Sandbox failed to block os library access!")
        }
        _ => {}
    }
}

/// Security: syntax errors are caught cleanly without panicking.
#[test]
fn test_syntax_error_is_caught() {
    let mut engine = AuwgentSandbox::new().unwrap();

    let bad_script = r#"
        local x = (((  -- syntax error
    "#;

    assert!(
        engine.execute(bad_script).is_err(),
        "Expected an Err for malformed syntax"
    );
}

/// Security: infinite loop is killed by the instruction limiter.
#[test]
fn test_infinite_loop_protection() {
    let mut engine = AuwgentSandbox::new().unwrap();

    let bad_script = r#"
        local x = 0
        while true do
            x = x + 1
        end
    "#;

    match engine.execute(bad_script) {
        Err(e) => {
            assert!(
                e.to_string().contains("Instruction limit exceeded"),
                "Wrong error: {}",
                e
            );
        }
        _ => panic!("Engine failed to stop the infinite loop!"),
    }
}

/// Prompt generation: verify ToolDefinition descriptions compose correctly.
#[test]
fn test_generate_tool_prompt() {
    let tools = vec![
        make_tool(
            "get_weather",
            "Returns current weather for a location",
            true,
            Some("{ location: string, units: string }"),
        ),
        make_tool("get_time", "Returns current server time", false, None),
    ];

    let prompt = AuwgentSandbox::generate_tool_prompt(&tools);

    assert!(prompt.contains("get_weather"));
    assert!(prompt.contains("Returns current weather for a location"));
    assert!(prompt.contains("{ location: string, units: string }"));
    assert!(prompt.contains("get_time"));
    assert!(prompt.contains("no arguments"));
}

/// The instruction counter is correctly reset between sequential execute() calls.
#[test]
fn test_instruction_counter_resets_between_executions() {
    let mut engine = AuwgentSandbox::new().unwrap();

    // A script that runs a heavy-but-legal loop
    let heavy_script = r#"
        local sum = 0
        for i = 1, 1000 do
            sum = sum + i
        end
        return tostring(sum)
    "#;

    // Run it multiple times — each should succeed independently
    for _ in 0..3 {
        match engine.execute(heavy_script).unwrap() {
            ExecutionResult::Finished { ret_val, .. } => {
                assert_eq!(ret_val.as_deref(), Some("500500"));
            }
            other => panic!("Expected Finished, got {:?}", other),
        }
    }
}

/// Boolean values print as "true"/"false", not "boolean" type name.
#[test]
fn test_boolean_print_coercion() {
    let mut engine = AuwgentSandbox::new().unwrap();

    let script = r#"
        print(true)
        print(false)
        return "ok"
    "#;

    match engine.execute(script).unwrap() {
        ExecutionResult::Finished { console_output, .. } => {
            assert!(console_output.contains("true\n"));
            assert!(console_output.contains("false\n"));
            assert!(!console_output.contains("boolean"));
        }
        other => panic!("Expected Finished, got {:?}", other),
    }
}

/// Default trait: AuwgentSandbox::default() works without panicking.
#[test]
fn test_sandbox_default_trait() {
    let mut engine = AuwgentSandbox::default();
    match engine.execute("return 1").unwrap() {
        ExecutionResult::Finished { ret_val, .. } => {
            assert_eq!(ret_val.as_deref(), Some("1"));
        }
        other => panic!("Expected Finished, got {:?}", other),
    }
}

/// Partial failure: three tools yielded, one fails — script handles it via __error sentinel.
/// The other two tools must still succeed and be accessible to the script.
#[test]
fn test_partial_tool_failure_via_tool_result() {
    let mut engine = AuwgentSandbox::new().unwrap();
    engine
        .register_tools(&[
            make_tool("get_weather", "Get weather", true, None),
            make_tool("get_stocks", "Get stocks", true, None),
            make_tool("get_news", "Get news", false, None),
        ])
        .unwrap();

    // The LLM calls three tools in one await_all.
    // It handles failures gracefully using the __error sentinel.
    let script = r#"
        local weather, stocks, news = await_all(
            get_weather({ location = "NY" }),
            get_stocks({ ticker = "AAPL" }),
            get_news()
        )

        if stocks.__error then
            print("Stocks failed:", stocks.message)
        else
            print("Stock price:", stocks.price)
        end

        print("Weather:", weather.condition)
        print("News:", news.headline)
        return "handled"
    "#;

    match engine.execute(script).unwrap() {
        ExecutionResult::YieldedForTools { tools } => {
            assert_eq!(tools.len(), 3);

            // Tool 1 succeeds, Tool 2 fails (rate limited), Tool 3 succeeds
            let results = vec![
                ToolResult::Ok(serde_json::json!({ "condition": "Sunny" })),
                ToolResult::Err("Rate limited: retry in 60s".into()),
                ToolResult::Ok(serde_json::json!({ "headline": "Rust is amazing" })),
            ];

            match engine.resume_with_results(results).unwrap() {
                ExecutionResult::Finished { ret_val, console_output } => {
                    assert_eq!(ret_val.as_deref(), Some("handled"));
                    // Error was handled gracefully — not a crash
                    assert!(console_output.contains("Stocks failed:"));
                    assert!(console_output.contains("Rate limited: retry in 60s"));
                    // Other two tools still delivered their values correctly
                    assert!(console_output.contains("Weather:\tSunny"));
                    assert!(console_output.contains("News:\tRust is amazing"));
                }
                other => panic!("Expected Finished, got {:?}", other),
            }
        }
        other => panic!("Expected YieldedForTools, got {:?}", other),
    }
}

/// All tools fail: script handles every __error in a batch.
#[test]
fn test_all_tools_fail_gracefully() {
    let mut engine = AuwgentSandbox::new().unwrap();
    engine
        .register_tools(&[
            make_tool("get_data_a", "Data A", false, None),
            make_tool("get_data_b", "Data B", false, None),
        ])
        .unwrap();

    let script = r#"
        local a, b = await_all(get_data_a(), get_data_b())

        local errors = 0
        if a.__error then errors = errors + 1 end
        if b.__error then errors = errors + 1 end

        print("Total errors:", errors)
        return tostring(errors)
    "#;

    match engine.execute(script).unwrap() {
        ExecutionResult::YieldedForTools { .. } => {
            let results = vec![
                ToolResult::Err("Service A is down".into()),
                ToolResult::Err("Service B is down".into()),
            ];

            match engine.resume_with_results(results).unwrap() {
                ExecutionResult::Finished { ret_val, console_output } => {
                    assert_eq!(ret_val.as_deref(), Some("2"));
                    assert!(console_output.contains("Total errors:\t2"));
                }
                other => panic!("Expected Finished, got {:?}", other),
            }
        }
        other => panic!("Expected YieldedForTools, got {:?}", other),
    }
}

// ─── Snapshot Tests ───────────────────────────────────────────────────────────

/// Snapshot taken right after the first yield. On restore the engine fast-forwards
/// through round 0 and surfaces the SECOND yield, ready for real execution.
#[test]
fn test_snapshot_restore_fast_forwards_to_next_yield() {
    let tools = vec![
        make_tool("step_a", "Step A", false, None),
        make_tool("step_b", "Step B", false, None),
    ];

    // ── Original session ────────────────────────────────────────────────────
    let mut engine = AuwgentSandbox::new().unwrap();
    engine.register_tools(&tools).unwrap();

    let script = r#"
        local a = await_all(step_a())
        local b = await_all(step_b())
        print("A:", a.val)
        print("B:", b.val)
        return "done"
    "#;

    // Round 0: step_a yields
    let status = engine.execute(script).unwrap();
    assert!(matches!(status, ExecutionResult::YieldedForTools { .. }));

    // Complete round 0
    engine
        .resume_with_json(vec![serde_json::json!({ "val": "alpha" })])
        .unwrap();

    // round 1 is now the active yield — snapshot HERE
    let snap = engine.snapshot().expect("snapshot() must return Some after execute()");

    // Verify snapshot contents
    assert_eq!(snap.script_source, script);
    assert_eq!(snap.completed_tool_results.len(), 1); // one round recorded
    assert_eq!(snap.tool_definitions.len(), 2);

    // ── Restore session ─────────────────────────────────────────────────────
    let (mut restored, status) = AuwgentSandbox::from_snapshot(snap).unwrap();

    // The engine fast-forwarded round 0 and is now sitting at round 1 (step_b)
    match status {
        ExecutionResult::YieldedForTools { tools } => {
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].tool_name, "step_b");

            // Complete round 1 with real (or mock) data
            match restored
                .resume_with_json(vec![serde_json::json!({ "val": "beta" })])
                .unwrap()
            {
                ExecutionResult::Finished { ret_val, console_output } => {
                    assert_eq!(ret_val.as_deref(), Some("done"));
                    // a.val was cached from round 0, b.val from the real resume
                    assert!(console_output.contains("A:\talpha"));
                    assert!(console_output.contains("B:\tbeta"));
                }
                other => panic!("Expected Finished, got {:?}", other),
            }
        }
        other => panic!("Expected YieldedForTools after restore, got {:?}", other),
    }
}

/// Snapshot taken after ALL yields have been completed. Restore should return
/// Finished immediately — no further yields or tool calls needed.
#[test]
fn test_snapshot_restore_after_all_rounds_returns_finished() {
    let tools = vec![make_tool("get_data", "Get data", false, None)];

    let mut engine = AuwgentSandbox::new().unwrap();
    engine.register_tools(&tools).unwrap();

    let script = r#"
        local d = await_all(get_data())
        return d.value
    "#;

    match engine.execute(script).unwrap() {
        ExecutionResult::YieldedForTools { .. } => {
            engine
                .resume_with_json(vec![serde_json::json!({ "value": "42" })])
                .unwrap();
        }
        other => panic!("Expected yield, got {:?}", other),
    }

    // Snapshot AFTER the script has finished — completed_tool_results has 1 round
    let snap = engine.snapshot().unwrap();
    assert_eq!(snap.completed_tool_results.len(), 1);

    // Restore: engine replays round 0 from cache, script reaches Finished with no new yield
    let (_, status) = AuwgentSandbox::from_snapshot(snap).unwrap();
    match status {
        ExecutionResult::Finished { ret_val, .. } => {
            assert_eq!(ret_val.as_deref(), Some("42"));
        }
        other => panic!("Expected Finished after full-cache restore, got {:?}", other),
    }
}

/// Snapshot correctly preserves ToolResult::Err sentinel tables across restore.
/// The `__error` table in round 0 must be faithfully reproduced in the restored VM.
#[test]
fn test_snapshot_preserves_error_sentinel_across_restore() {
    let tools = vec![
        make_tool("flaky_tool", "Flaky", false, None),
        make_tool("final_tool", "Final", false, None),
    ];

    let mut engine = AuwgentSandbox::new().unwrap();
    engine.register_tools(&tools).unwrap();

    let script = r#"
        local result = await_all(flaky_tool())
        if result.__error then
            print("Error captured:", result.message)
        end
        local b = await_all(final_tool())
        return b.status
    "#;

    // Round 0: inject an error via ToolResult::Err
    match engine.execute(script).unwrap() {
        ExecutionResult::YieldedForTools { .. } => {
            engine
                .resume_with_results(vec![ToolResult::Err("timeout after 30s".into())])
                .unwrap();
        }
        other => panic!("{:?}", other),
    }

    // Snapshot after round 0 (error round)
    let snap = engine.snapshot().unwrap();

    // Verify the error was stored as JSON sentinel, not as ToolResult enum
    let stored = &snap.completed_tool_results[0][0];
    assert_eq!(stored["__error"], serde_json::json!(true));
    assert_eq!(stored["message"], serde_json::json!("timeout after 30s"));

    // Restore — fast-forwards round 0 (the error), sits at final_tool yield
    let (mut restored, status) = AuwgentSandbox::from_snapshot(snap).unwrap();

    match status {
        ExecutionResult::YieldedForTools { tools } => {
            assert_eq!(tools[0].tool_name, "final_tool");

            match restored
                .resume_with_json(vec![serde_json::json!({ "status": "ok" })])
                .unwrap()
            {
                ExecutionResult::Finished { ret_val, console_output } => {
                    assert_eq!(ret_val.as_deref(), Some("ok"));
                    // Proves __error table was faithfully reproduced after restore
                    assert!(console_output.contains("Error captured:\ttimeout after 30s"));
                }
                other => panic!("{:?}", other),
            }
        }
        other => panic!("{:?}", other),
    }
}

/// load_library: pre-written Lua utility functions are available in the LLM
/// script without the LLM needing to redefine them.
#[test]
fn test_load_library_functions_available_in_script() {
    let mut engine = AuwgentSandbox::new().unwrap();

    // Load a utility library BEFORE the LLM script
    engine
        .load_library(
            r#"
        function format_currency(amount, symbol)
            return symbol .. string.format("%.2f", amount)
        end

        function clamp(val, min_val, max_val)
            return math.max(min_val, math.min(max_val, val))
        end
    "#,
        )
        .unwrap();

    let script = r#"
        local price = format_currency(19.9, "$")
        local clamped = clamp(150, 0, 100)
        print("Price:", price)
        print("Clamped:", clamped)
        return price
    "#;

    match engine.execute(script).unwrap() {
        ExecutionResult::Finished { ret_val, console_output } => {
            assert_eq!(ret_val.as_deref(), Some("$19.90"));
            assert!(console_output.contains("Price:\t$19.90"));
            assert!(console_output.contains("Clamped:\t100"));
        }
        other => panic!("Expected Finished, got {:?}", other),
    }
}

/// load_library functions survive snapshot/restore — they are reloaded
/// from the snapshot.libraries Vec and available in the restored engine.
#[test]
fn test_load_library_survives_snapshot_restore() {
    let tools = vec![make_tool("get_price", "Get price", false, None)];

    let mut engine = AuwgentSandbox::new().unwrap();
    engine
        .load_library(
            r#"
        function format_currency(amount, symbol)
            return symbol .. string.format("%.2f", amount)
        end
    "#,
        )
        .unwrap();
    engine.register_tools(&tools).unwrap();

    let script = r#"
        local data = await_all(get_price())
        return format_currency(data.price, "€")
    "#;

    match engine.execute(script).unwrap() {
        ExecutionResult::YieldedForTools { .. } => {
            // Snapshot before resolving
            let snap = engine.snapshot().unwrap();
            assert_eq!(snap.libraries.len(), 1);

            // Restore — fast-forwards nothing (0 rounds completed), sits at get_price yield
            let (mut restored, status) = AuwgentSandbox::from_snapshot(snap).unwrap();

            match status {
                ExecutionResult::YieldedForTools { tools } => {
                    assert_eq!(tools[0].tool_name, "get_price");

                    match restored
                        .resume_with_json(vec![serde_json::json!({ "price": 9.5 })])
                        .unwrap()
                    {
                        ExecutionResult::Finished { ret_val, .. } => {
                            // format_currency must be available after restore
                            assert_eq!(ret_val.as_deref(), Some("€9.50"));
                        }
                        other => panic!("{:?}", other),
                    }
                }
                other => panic!("{:?}", other),
            }
        }
        other => panic!("{:?}", other),
    }
}

// ─── Coroutine Depth Tests ────────────────────────────────────────────────────

/// Proves that await_all works correctly when called from inside nested Lua
/// function calls. The coroutine suspends the ENTIRE thread stack — local
/// variables at every frame survive the yield/resume cycle transparently.
#[test]
fn test_await_all_works_inside_nested_functions() {
    let mut engine = AuwgentSandbox::new().unwrap();
    engine
        .register_tools(&[make_tool("get_item", "Get item by id", true, None)])
        .unwrap();

    // Three levels of function nesting, with await_all at the deepest level.
    // This is the same pattern a coding agent would use: helper functions
    // that transparently delegate to tools without the caller knowing.
    engine
        .load_library(
            r#"
        -- Level 3 (deepest): calls await_all directly
        local function fetch_detail(id)
            local data = await_all(get_item({ id = id }))
            -- These locals must survive the yield/resume cycle
            return data.label .. ":" .. tostring(data.score)
        end

        -- Level 2: calls into level 3
        local function process_item(id)
            local detail = fetch_detail(id)   -- transparent — no yield syntax needed here
            return "[" .. detail .. "]"
        end

        -- Level 1: loops and calls level 2 per item
        function build_report(ids)
            local parts = {}
            for _, id in ipairs(ids) do
                table.insert(parts, process_item(id))
            end
            return table.concat(parts, ", ")
        end
    "#,
        )
        .unwrap();

    let script = r#"
        local report = build_report({ "a", "b", "c" })
        print("Report:", report)
        return report
    "#;

    let mut call_count = 0usize;
    let result = run_to_finish(&mut engine, script, |name, payload| {
        assert_eq!(name, "get_item");
        call_count += 1;
        let id = payload["id"].as_str().unwrap_or("");
        // Each call returns a unique score so we can verify all three survived correctly
        serde_json::json!({ "label": format!("item_{}", id), "score": call_count })
    });

    // await_all was called 3 times — once per item, from 3 levels deep
    assert_eq!(call_count, 3);

    match result {
        ExecutionResult::Finished { ret_val, console_output } => {
            let report = ret_val.unwrap_or_default();
            // Verify each item was enriched correctly — locals at all stack frames survived
            assert!(report.contains("[item_a:1]"), "got: {}", report);
            assert!(report.contains("[item_b:2]"), "got: {}", report);
            assert!(report.contains("[item_c:3]"), "got: {}", report);
            assert!(console_output.contains("Report:"));
        }
        other => panic!("Expected Finished, got {:?}", other),
    }
}

/// Proves that a yielded tool called inside a deeply nested closure (not just
/// a named function) also suspends and resumes correctly.
#[test]
fn test_await_all_works_inside_closure() {
    let mut engine = AuwgentSandbox::new().unwrap();
    engine
        .register_tools(&[make_tool("transform", "Transform value", true, None)])
        .unwrap();

    // The closure is created and stored inside a table — a more complex capture scenario
    engine
        .load_library(
            r#"
        function make_pipeline(multiplier)
            -- Returns a closure that captures `multiplier` as an upvalue
            return function(val)
                local result = await_all(transform({ value = val * multiplier }))
                return result.transformed
            end
        end
    "#,
        )
        .unwrap();

    let script = r#"
        local pipeline = make_pipeline(10)
        local out = pipeline(5)   -- 5 * 10 = 50 sent to transform
        print("Output:", out)
        return tostring(out)
    "#;

    let result = run_to_finish(&mut engine, script, |_, payload| {
        let v = payload["value"].as_f64().unwrap_or(0.0);
        // The value sent should be 50 (5 * 10), proving the closure upvalue survived yield
        serde_json::json!({ "transformed": v + 1.0 })
    });

    match result {
        ExecutionResult::Finished { ret_val, console_output } => {
            // 5 * 10 = 50, then transform adds 1 → 51
            // Luau treats 50 + 1.0 as integer when inputs are whole numbers
            assert_eq!(ret_val.as_deref(), Some("51"));
            assert!(console_output.contains("Output:\t51"));
        }
        other => panic!("Expected Finished, got {:?}", other),
    }
}
