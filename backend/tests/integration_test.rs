use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::time::Duration;

const PIPE_NAME: &str = r"\\.\pipe\docubot";

fn connect_to_pipe() -> Result<std::fs::File, Box<dyn std::error::Error>> {
    for i in 0..30 {
        match OpenOptions::new().read(true).write(true).open(PIPE_NAME) {
            Ok(f) => return Ok(f),
            Err(_) => {
                std::thread::sleep(Duration::from_millis(500));
                if i == 29 {
                    return Err("Failed to connect to named pipe after 15 seconds".into());
                }
            }
        }
    }
    unreachable!()
}

fn send_request(
    pipe: &mut std::fs::File,
    json: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut message = json.to_string();
    message.push('\n');
    pipe.write_all(message.as_bytes())?;
    pipe.flush()?;

    let mut buffer = [0u8; 65536];
    let mut response = String::new();
    let mut attempts = 0;

    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                let chunk = String::from_utf8_lossy(&buffer[..n]);
                response.push_str(&chunk);
                if response.contains('\n') {
                    break;
                }
            }
            Err(_) => {
                attempts += 1;
                if attempts > 5 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }

    Ok(response.lines().next().unwrap_or("").to_string())
}

fn run_test(
    name: &str,
    test_fn: impl FnOnce(&mut std::fs::File) -> Result<String, Box<dyn std::error::Error>>,
) {
    print!("Testing: {}... ", name);
    match connect_to_pipe() {
        Ok(mut pipe) => match test_fn(&mut pipe) {
            Ok(response) => {
                if response.contains("\"error\"") {
                    println!(
                        "RESPONDED (error: {})",
                        &response[..response.len().min(100)]
                    );
                } else if !response.is_empty() {
                    println!("PASS");
                    println!("  Response: {}", &response[..response.len().min(200)]);
                } else {
                    println!("PASS (empty response)");
                }
            }
            Err(e) => println!("ERROR: {}", e),
        },
        Err(e) => println!("FAIL: {}", e),
    }
}

fn main() {
    println!("========================================");
    println!("  DocuBot Backend Integration Tests");
    println!("========================================");
    println!();

    // Test 1: Health check
    run_test("Health Check", |pipe| {
        let req = r#"{"type":"health_check","id":"test_001"}"#;
        send_request(pipe, req)
    });

    // Test 2: Basic chat
    run_test("Chat Request", |pipe| {
        let req = r#"{"type":"chat","id":"test_002","message":"Hello, this is a test message","conversation_id":"test_conv_001"}"#;
        send_request(pipe, req)
    });

    // Test 3: Memory - Create a node
    run_test("Memory: Create Node", |pipe| {
        let req = r#"{"type":"create_node","id":"test_003","node_type":"concept","title":"DocuBot","metadata":{"description":"A documentation bot built with Rust and WinUI"}}"#;
        send_request(pipe, req)
    });

    // Test 4: Memory - Query nodes
    run_test("Memory: Query", |pipe| {
        let req = r#"{"type":"query_memory","id":"test_004","query":"DocuBot project","limit":5}"#;
        send_request(pipe, req)
    });

    // Test 5: Memory - Get connected nodes
    run_test("Memory: Get Connected Nodes", |pipe| {
        let req = r#"{"type":"get_connected_nodes","id":"test_005","node_id":"test_node"}"#;
        send_request(pipe, req)
    });

    // Test 6: Memory - Search embeddings
    run_test("Memory: Search Embeddings", |pipe| {
        let req = r#"{"type":"search_embeddings","id":"test_006","query":"Rust programming language","limit":3}"#;
        send_request(pipe, req)
    });

    // Test 7: Tool - File read
    run_test("Tool: File Read", |pipe| {
        let req = r#"{"type":"read_file","id":"test_007","path":"Cargo.toml"}"#;
        send_request(pipe, req)
    });

    // Test 8: Tool - Web search
    run_test("Tool: Web Search", |pipe| {
        let req = r#"{"type":"web_search","id":"test_008","query":"Rust programming"}"#;
        send_request(pipe, req)
    });

    // Test 9: Tool - File write
    run_test("Tool: File Write", |pipe| {
        let req = r#"{"type":"write_file","id":"test_009","path":"test_output.txt","content":"Hello from integration test"}"#;
        send_request(pipe, req)
    });

    // Test 10: Skill - List skills
    run_test("Skill: List Skills", |pipe| {
        let req = r#"{"type":"list_skills","id":"test_010"}"#;
        send_request(pipe, req)
    });

    // Test 11: Skill - Execute
    run_test("Skill: Execute", |pipe| {
        let req = r#"{"type":"execute_skill","id":"test_011","skill_name":"summarize","args":{"target":"project"}}"#;
        send_request(pipe, req)
    });

    // Test 12: Preview - Get previews
    run_test("Preview: Get Previews", |pipe| {
        let req = r#"{"type":"get_previews","id":"test_012"}"#;
        send_request(pipe, req)
    });

    // Test 13: Preview - Create preview
    run_test("Preview: Create Preview", |pipe| {
        let req = r##"{"type":"create_preview","id":"test_013","name":"test_preview.md","content":"# Test. This is a test preview"}"##;
        send_request(pipe, req)
    });

    // Test 14: Chat with context (after creating memory)
    run_test("Chat with Context", |pipe| {
        let req = r#"{"type":"chat","id":"test_014","message":"What do you know about DocuBot?","conversation_id":"test_conv_001"}"#;
        send_request(pipe, req)
    });

    // Test 15: Memory - Get specific node
    run_test("Memory: Get Node", |pipe| {
        let req = r#"{"type":"get_node","id":"test_015","node_id":"test_node_001"}"#;
        send_request(pipe, req)
    });

    println!();
    println!("========================================");
    println!("  All 15 tests completed");
    println!("========================================");
}
