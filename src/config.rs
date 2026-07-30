use serde::{Deserialize, Serialize};
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
        // Try to load from ~/.tuisample-code/config.toml
        let config_path = Self::config_path();

        if config_path.exists() {
            let contents = std::fs::read_to_string(&config_path)?;
            Ok(toml::from_str(&contents)?)
        } else {
            // Return default config
            Ok(Self::default())
        }
    }

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
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
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
