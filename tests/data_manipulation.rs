/// Integration tests focused on list and data manipulation inside the Lua sandbox.
/// These simulate realistic AI agent scenarios where tools return structured collections
/// and the LLM script performs transformations before returning or calling further tools.
use auwgent_mode::{AuwgentSandbox, ExecutionResult, ToolDefinition};

// ─── Shared Helper ────────────────────────────────────────────────────────────

fn make_tool(name: &str, has_args: bool) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: name.to_string(),
        has_args,
        arg_schema: None,
    }
}

/// Drive the sandbox to completion, routing all yields through `dispatcher`.
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

/// Filter a list of users returned from a tool, keeping only those over 30.
#[test]
fn test_filter_list_by_age() {
    let mut engine = AuwgentSandbox::new().unwrap();
    engine
        .register_tools(&[make_tool("get_users", false)])
        .unwrap();

    let script = r#"
        local users = await_all(get_users())

        local adults = {}
        for _, user in ipairs(users) do
            if user.age > 30 then
                table.insert(adults, user.name)
            end
        end

        print("Filtered count:", #adults)
        print("Names:", table.concat(adults, ", "))
        return tostring(#adults)
    "#;

    let result = run_to_finish(&mut engine, script, |name, _| {
        assert_eq!(name, "get_users");
        serde_json::json!([
            { "name": "Alice", "age": 25 },
            { "name": "Bob",   "age": 35 },
            { "name": "Carol", "age": 42 },
            { "name": "Dave",  "age": 28 }
        ])
    });

    match result {
        ExecutionResult::Finished { ret_val, console_output, .. } => {
            // Bob and Carol are over 30
            assert_eq!(ret_val.as_deref(), Some("2"));
            assert!(console_output.contains("Filtered count:\t2"));
            assert!(console_output.contains("Bob"));
            assert!(console_output.contains("Carol"));
        }
        other => panic!("Expected Finished, got {:?}", other),
    }
}

/// Sort products by price and return the cheapest one.
#[test]
fn test_sort_list_and_pick_min() {
    let mut engine = AuwgentSandbox::new().unwrap();
    engine
        .register_tools(&[make_tool("get_products", false)])
        .unwrap();

    let script = r#"
        local products = await_all(get_products())

        -- Sort by price ascending
        table.sort(products, function(a, b) return a.price < b.price end)

        local cheapest = products[1]
        print("Cheapest:", cheapest.name, "at", cheapest.price)
        return cheapest.name
    "#;

    let result = run_to_finish(&mut engine, script, |_, _| {
        serde_json::json!([
            { "name": "Laptop",  "price": 999 },
            { "name": "Mouse",   "price": 29  },
            { "name": "Monitor", "price": 349 },
            { "name": "Keyboard","price": 79  }
        ])
    });

    match result {
        ExecutionResult::Finished { ret_val, console_output, .. } => {
            assert_eq!(ret_val.as_deref(), Some("Mouse"));
            assert!(console_output.contains("Mouse"));
            assert!(console_output.contains("29"));
        }
        other => panic!("Expected Finished, got {:?}", other),
    }
}

/// Aggregate: sum and average a list of scores returned by a tool.
#[test]
fn test_aggregate_sum_and_average() {
    let mut engine = AuwgentSandbox::new().unwrap();
    engine
        .register_tools(&[make_tool("get_scores", false)])
        .unwrap();

    let script = r#"
        local scores = await_all(get_scores())

        local total = 0
        for _, s in ipairs(scores) do
            total = total + s
        end

        local avg = total / #scores
        print("Total:", total)
        print("Average:", avg)
        print("Count:", #scores)
        return tostring(total)
    "#;

    let result = run_to_finish(&mut engine, script, |_, _| {
        serde_json::json!([10, 20, 30, 40, 50])
    });

    match result {
        ExecutionResult::Finished { ret_val, console_output, .. } => {
            assert_eq!(ret_val.as_deref(), Some("150"));
            assert!(console_output.contains("Total:\t150"));
            assert!(console_output.contains("Average:\t30"));
            assert!(console_output.contains("Count:\t5"));
        }
        other => panic!("Expected Finished, got {:?}", other),
    }
}

/// Merge two lists from separate tool calls into a single combined result.
#[test]
fn test_merge_two_lists() {
    let mut engine = AuwgentSandbox::new().unwrap();
    engine
        .register_tools(&[
            make_tool("get_team_a", false),
            make_tool("get_team_b", false),
        ])
        .unwrap();

    let script = r#"
        local team_a, team_b = await_all(get_team_a(), get_team_b())

        local all_members = {}
        for _, m in ipairs(team_a) do table.insert(all_members, m) end
        for _, m in ipairs(team_b) do table.insert(all_members, m) end

        print("Total members:", #all_members)
        return tostring(#all_members)
    "#;

    let result = run_to_finish(&mut engine, script, |name, _| match name {
        "get_team_a" => serde_json::json!(["Alice", "Bob", "Carol"]),
        "get_team_b" => serde_json::json!(["Dave", "Eve"]),
        other => panic!("Unexpected tool: {}", other),
    });

    match result {
        ExecutionResult::Finished { ret_val, console_output, .. } => {
            assert_eq!(ret_val.as_deref(), Some("5"));
            assert!(console_output.contains("Total members:\t5"));
        }
        other => panic!("Expected Finished, got {:?}", other),
    }
}

/// Sequential per-item calls: get a list of IDs, then enrich each one with a tool call.
/// Demonstrates that the LLM can loop over data and yield multiple times.
#[test]
fn test_sequential_per_item_tool_calls() {
    let mut engine = AuwgentSandbox::new().unwrap();
    engine
        .register_tools(&[
            make_tool("get_ids", false),
            make_tool("enrich_id", true),
        ])
        .unwrap();

    let script = r#"
        local ids = await_all(get_ids())

        local results = {}
        for _, id in ipairs(ids) do
            local detail = await_all(enrich_id({ id = id }))
            table.insert(results, detail.label)
        end

        print("Enriched:", table.concat(results, " | "))
        return tostring(#results)
    "#;

    let mut enrich_call_count = 0usize;
    let result = run_to_finish(&mut engine, script, |name, payload| match name {
        "get_ids" => serde_json::json!(["id_1", "id_2", "id_3"]),
        "enrich_id" => {
            enrich_call_count += 1;
            let id = payload["id"].as_str().unwrap_or("");
            serde_json::json!({ "label": format!("Label({})", id) })
        }
        other => panic!("Unexpected: {}", other),
    });

    // enrich_id must have been called exactly 3 times (once per ID)
    assert_eq!(enrich_call_count, 3);

    match result {
        ExecutionResult::Finished { ret_val, console_output, .. } => {
            assert_eq!(ret_val.as_deref(), Some("3"));
            assert!(console_output.contains("Label(id_1)"));
            assert!(console_output.contains("Label(id_2)"));
            assert!(console_output.contains("Label(id_3)"));
        }
        other => panic!("Expected Finished, got {:?}", other),
    }
}

/// Find the maximum value in a list.
#[test]
fn test_find_max_in_list() {
    let mut engine = AuwgentSandbox::new().unwrap();
    engine
        .register_tools(&[make_tool("get_readings", false)])
        .unwrap();

    let script = r#"
        local readings = await_all(get_readings())

        local max_val = readings[1]
        for _, v in ipairs(readings) do
            if v > max_val then max_val = v end
        end

        print("Max reading:", max_val)
        return tostring(max_val)
    "#;

    let result = run_to_finish(&mut engine, script, |_, _| {
        serde_json::json!([42, 17, 99, 3, 56, 88, 12])
    });

    match result {
        ExecutionResult::Finished { ret_val, console_output, .. } => {
            assert_eq!(ret_val.as_deref(), Some("99"));
            assert!(console_output.contains("Max reading:\t99"));
        }
        other => panic!("Expected Finished, got {:?}", other),
    }
}

/// Nested table navigation: deeply access a structured return value.
#[test]
fn test_nested_table_navigation() {
    let mut engine = AuwgentSandbox::new().unwrap();
    engine
        .register_tools(&[make_tool("get_company", false)])
        .unwrap();

    let script = r#"
        local company = await_all(get_company())

        local city = company.headquarters.address.city
        local ceo_name = company.leadership.ceo.name

        print("City:", city)
        print("CEO:", ceo_name)
        print("Revenue:", company.financials.annual_revenue)
        return city
    "#;

    let result = run_to_finish(&mut engine, script, |_, _| {
        serde_json::json!({
            "name": "Auwgent Corp",
            "headquarters": {
                "address": { "city": "Lagos", "country": "Nigeria" }
            },
            "leadership": {
                "ceo": { "name": "Ada Obi", "age": 38 }
            },
            "financials": {
                "annual_revenue": 5_000_000
            }
        })
    });

    match result {
        ExecutionResult::Finished { ret_val, console_output, .. } => {
            assert_eq!(ret_val.as_deref(), Some("Lagos"));
            assert!(console_output.contains("Lagos"));
            assert!(console_output.contains("Ada Obi"));
            assert!(console_output.contains("5000000"));
        }
        other => panic!("Expected Finished, got {:?}", other),
    }
}

/// Count items by category (group-by simulation in Lua).
#[test]
fn test_group_and_count_by_category() {
    let mut engine = AuwgentSandbox::new().unwrap();
    engine
        .register_tools(&[make_tool("get_orders", false)])
        .unwrap();

    let script = r#"
        local orders = await_all(get_orders())

        local counts = {}
        for _, order in ipairs(orders) do
            local cat = order.category
            counts[cat] = (counts[cat] or 0) + 1
        end

        print("Electronics:", counts["Electronics"])
        print("Clothing:", counts["Clothing"])
        print("Food:", counts["Food"])
        return tostring(counts["Electronics"])
    "#;

    let result = run_to_finish(&mut engine, script, |_, _| {
        serde_json::json!([
            { "id": 1, "category": "Electronics" },
            { "id": 2, "category": "Clothing" },
            { "id": 3, "category": "Electronics" },
            { "id": 4, "category": "Food" },
            { "id": 5, "category": "Electronics" },
            { "id": 6, "category": "Clothing" }
        ])
    });

    match result {
        ExecutionResult::Finished { ret_val, console_output, .. } => {
            assert_eq!(ret_val.as_deref(), Some("3"));
            assert!(console_output.contains("Electronics:\t3"));
            assert!(console_output.contains("Clothing:\t2"));
            assert!(console_output.contains("Food:\t1"));
        }
        other => panic!("Expected Finished, got {:?}", other),
    }
}

/// Build a formatted report string from a list, then pass the result to a second tool.
#[test]
fn test_build_report_and_post_to_tool() {
    let mut engine = AuwgentSandbox::new().unwrap();
    engine
        .register_tools(&[
            make_tool("get_metrics", false),
            make_tool("post_report", true),
        ])
        .unwrap();

    let script = r#"
        local metrics = await_all(get_metrics())

        -- Build a compact report string from the list
        local lines = {}
        for _, m in ipairs(metrics) do
            table.insert(lines, m.key .. "=" .. tostring(m.value))
        end
        local report = table.concat(lines, ";")

        -- Post the formatted report to the host
        local ack = await_all(post_report({ body = report }))
        print("Ack:", ack.status)
        return report
    "#;

    let mut posted_body = String::new();
    let result = run_to_finish(&mut engine, script, |name, payload| match name {
        "get_metrics" => serde_json::json!([
            { "key": "cpu", "value": 72 },
            { "key": "mem", "value": 48 },
            { "key": "disk","value": 91 }
        ]),
        "post_report" => {
            posted_body = payload["body"].as_str().unwrap_or("").to_string();
            serde_json::json!({ "status": "accepted" })
        }
        other => panic!("Unexpected: {}", other),
    });

    assert!(posted_body.contains("cpu=72"));
    assert!(posted_body.contains("mem=48"));
    assert!(posted_body.contains("disk=91"));

    match result {
        ExecutionResult::Finished { console_output, .. } => {
            assert!(console_output.contains("accepted"));
        }
        other => panic!("Expected Finished, got {:?}", other),
    }
}

/// Deduplicate a list: remove repeated items returned by two tool calls.
#[test]
fn test_deduplicate_merged_lists() {
    let mut engine = AuwgentSandbox::new().unwrap();
    engine
        .register_tools(&[
            make_tool("get_list_a", false),
            make_tool("get_list_b", false),
        ])
        .unwrap();

    let script = r#"
        local list_a, list_b = await_all(get_list_a(), get_list_b())

        local seen = {}
        local unique = {}
        for _, v in ipairs(list_a) do
            if not seen[v] then seen[v] = true; table.insert(unique, v) end
        end
        for _, v in ipairs(list_b) do
            if not seen[v] then seen[v] = true; table.insert(unique, v) end
        end

        table.sort(unique)
        print("Unique count:", #unique)
        print("Values:", table.concat(unique, ","))
        return tostring(#unique)
    "#;

    let result = run_to_finish(&mut engine, script, |name, _| match name {
        "get_list_a" => serde_json::json!(["apple", "banana", "cherry"]),
        "get_list_b" => serde_json::json!(["banana", "date", "apple", "elderberry"]),
        other => panic!("Unexpected: {}", other),
    });

    match result {
        ExecutionResult::Finished { ret_val, console_output, .. } => {
            // apple, banana, cherry, date, elderberry → 5 unique
            assert_eq!(ret_val.as_deref(), Some("5"));
            assert!(console_output.contains("Unique count:\t5"));
        }
        other => panic!("Expected Finished, got {:?}", other),
    }
}
