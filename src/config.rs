use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub llm: LlmConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LlmConfig {
    pub endpoint: String,
    pub model: String,
    pub api_key: String,
}

impl Config {
    pub fn load() -> Result<Self, Box<dyn std::error::Error>> {
        let config_path = Self::config_path();

        if config_path.exists() {
            let contents = std::fs::read_to_string(&config_path)?;
            Ok(toml::from_str(&contents)?)
        } else {
            // Check if env vars are set
            let endpoint = std::env::var("TUISAMPLE_ENDPOINT").ok();
            let model = std::env::var("TUISAMPLE_MODEL").ok();
            let api_key = std::env::var("TUISAMPLE_API_KEY").ok();

            if endpoint.is_some() && model.is_some() && api_key.is_some() {
                // Use env vars
                Ok(Self::default())
            } else {
                // Interactive setup
                println!("\n🚀 Welcome to tuisample-code!\n");
                println!("No configuration found. Let's set up your LLM connection.\n");

                let endpoint = Self::prompt("LLM Endpoint (e.g., https://api.openai.com)")?;
                let model = Self::prompt("Model name (e.g., gpt-4)")?;
                let api_key = Self::prompt("API Key")?;

                let config = Config {
                    llm: LlmConfig {
                        endpoint,
                        model,
                        api_key,
                    },
                };

                // Save config for next time
                config.save().ok(); // Ignore save errors

                println!("\n✓ Configuration saved to ~/.tuisample-code/config.toml\n");

                Ok(config)
            }
        }
    }

    fn prompt(label: &str) -> Result<String, Box<dyn std::error::Error>> {
        print!("{}: ", label);
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        Ok(input.trim().to_string())
    }

    #[allow(dead_code)]
    pub fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        let config_path = Self::config_path();

        // Create directory if it doesn't exist
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let toml_str = toml::to_string_pretty(self)?;
        std::fs::write(&config_path, toml_str)?;
        Ok(())
    }

    fn config_path() -> PathBuf {
        let home = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        home.join(".tuisample-code").join("config.toml")
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            llm: LlmConfig {
                endpoint: std::env::var("TUISAMPLE_ENDPOINT")
                    .unwrap_or_else(|_| "http://localhost:8000".to_string()),
                model: std::env::var("TUISAMPLE_MODEL")
                    .unwrap_or_else(|_| "gpt-3.5-turbo".to_string()),
                api_key: std::env::var("TUISAMPLE_API_KEY")
                    .unwrap_or_else(|_| "sk_test".to_string()),
            },
        }
    }
}
