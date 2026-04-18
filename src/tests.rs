use super::*;

#[test]
fn test_advanced_sandbox_loop() {
    let mut engine = AuwgentSandbox::new().unwrap();
    
    // Developer cleanly registers tools with structural definitions!
    engine.register_tools(&[
        ToolDefinition { name: "fetch_user".to_string(), has_args: true },
        ToolDefinition { name: "verify_status".to_string(), has_args: true },
    ]).unwrap();
    
    // An advanced script with multi-step reasoning
    let script = r#"
        print("Starting advanced multi-step task...")
        
        -- Step 1: Call first tool
        local user_data = await_all(fetch_user({ id = "user_777" }))
        print("Got user data!", user_data.name)
        
        -- Step 2: Do some manipulation natively in Lua
        local status_check = user_data.name .. "_check"
        
        -- Step 3: Call second tool based on the manipulation
        local status_res = await_all(verify_status({ target = status_check }))
        
        print("Final result:", status_res.verified)
        
        return "SUCCESS"
    "#;
    
    // We execute the engine loop!
    let mut current_status = engine.execute(script).unwrap();
    
    loop {
        match current_status {
            ExecutionResult::YieldedForTools { tools } => {
                let mut rust_responses = Vec::new();
                
                for tool in tools {
                    if tool.tool_name == "fetch_user" {
                        assert_eq!(tool.payload["id"], "user_777");
                        // We do host-work and inject real structured JSON back!
                        rust_responses.push(serde_json::json!({ "name": "JohnDoe", "age": 30 }));
                    } else if tool.tool_name == "verify_status" {
                        assert_eq!(tool.payload["target"], "JohnDoe_check");
                        rust_responses.push(serde_json::json!({ "verified": true }));
                    }
                }
                
                // We use our clean DX method to push JSON natively back into standard Lua variables!
                current_status = engine.resume_with_json(rust_responses).unwrap();
            },
            ExecutionResult::Finished { ret_val, console_output } => {
                assert_eq!(ret_val.unwrap(), "SUCCESS");
                assert!(console_output.contains("Got user data!\tJohnDoe"));
                if !console_output.contains("Final result:\ttrue") {
                    panic!("Console output was wrong:\n{}", console_output);
                }
                break;
            },
            ExecutionResult::Error(e) => panic!("Execution crashed: {}", e),
        }
    }
}
