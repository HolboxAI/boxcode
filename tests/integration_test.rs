// Basic integration tests for boxcode

#[test]
fn test_json_parsing() {
    // Test that we can parse OpenAI-format JSON responses
    let json = r#"{"choices":[{"delta":{"content":"Hello"}}]}"#;
    let result: Result<serde_json::Value, _> = serde_json::from_str(json);
    assert!(result.is_ok());

    let value = result.unwrap();
    assert!(value["choices"].is_array());
}

#[test]
fn test_env_var_loading() {
    // Test that environment variables are read correctly
    std::env::set_var("TEST_VAR", "test_value");
    let val = std::env::var("TEST_VAR");
    assert_eq!(val.unwrap(), "test_value");
}

#[test]
fn test_string_operations() {
    // Test basic string buffer operations
    let mut buffer = String::new();
    buffer.push_str("hello");
    assert_eq!(buffer, "hello");

    buffer.push_str(" world");
    assert_eq!(buffer, "hello world");
}
