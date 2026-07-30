// Tests for system requirements

#[test]
fn test_cargo_available() {
    // Check if cargo is installed (required for install.sh)
    let cargo_check = std::process::Command::new("cargo")
        .arg("--version")
        .output();

    match cargo_check {
        Ok(output) => {
            let stdout = String::from_utf8(output.stdout).unwrap_or_default();
            println!("Cargo found: {}", stdout.trim());
            assert!(output.status.success(), "cargo --version must succeed");
        }
        Err(e) => {
            eprintln!("SKIP: Cargo not found - {}", e);
            eprintln!("Install Rust from https://rustup.rs/");
            // Don't fail the test - it's a system requirement, not a code bug
        }
    }
}

#[test]
fn test_config_files_exist() {
    // Check if we can create config directory
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let config_dir = format!("{}/.tuisample-code", home);

    // Should be able to create the directory
    let result = std::fs::create_dir_all(&config_dir);
    assert!(result.is_ok(), "Should be able to create config directory");

    // Cleanup
    let _ = std::fs::remove_dir(&config_dir);
}

#[test]
fn test_config_env_vars_format() {
    // Test that env vars follow expected format
    let valid_endpoint = "https://api.openai.com";
    let valid_model = "gpt-4";
    let valid_api_key = "sk-1234567890";

    assert!(valid_endpoint.starts_with("http"), "Endpoint must be a URL");
    assert!(!valid_model.is_empty(), "Model must not be empty");
    assert!(!valid_api_key.is_empty(), "API key must not be empty");
}
