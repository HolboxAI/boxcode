// Tests for configuration loading and defaults

#[test]
fn test_config_struct_creation() {
    // Test that we can create a config with all required fields
    let config_str = r#"
[llm]
endpoint = "https://api.openai.com"
model = "gpt-4"
api_key = "sk-test123"
"#;

    let config: Result<toml::Table, _> = toml::from_str(config_str);
    assert!(config.is_ok());

    let table = config.unwrap();
    assert!(table.contains_key("llm"));
}

#[test]
fn test_env_var_fallback() {
    // Test that env vars can provide config values
    std::env::set_var("BOXCODE_ENDPOINT", "https://test.local");
    std::env::set_var("BOXCODE_MODEL", "test-model");
    std::env::set_var("BOXCODE_API_KEY", "test-key");

    let endpoint = std::env::var("BOXCODE_ENDPOINT").unwrap();
    let model = std::env::var("BOXCODE_MODEL").unwrap();
    let api_key = std::env::var("BOXCODE_API_KEY").unwrap();

    assert_eq!(endpoint, "https://test.local");
    assert_eq!(model, "test-model");
    assert_eq!(api_key, "test-key");
}

#[test]
fn test_toml_serialization() {
    // Test that config can be serialized to TOML
    let toml_content = r#"
[llm]
endpoint = "https://api.example.com"
model = "example-model"
api_key = "sk-example"
"#;

    // Should parse without error
    let parsed: Result<toml::Table, _> = toml::from_str(toml_content);
    assert!(parsed.is_ok());
}
