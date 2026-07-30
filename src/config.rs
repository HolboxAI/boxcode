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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default() {
        let config = Config::default();
        assert_eq!(config.llm.endpoint, "http://localhost:8000");
        assert_eq!(config.llm.model, "gpt-3.5-turbo");
        assert_eq!(config.llm.api_key, "sk_test");
    }

    #[test]
    fn test_llm_config_struct() {
        let llm_config = LlmConfig {
            endpoint: "https://api.example.com".to_string(),
            model: "custom-model".to_string(),
            api_key: "sk_custom".to_string(),
        };

        assert_eq!(llm_config.endpoint, "https://api.example.com");
        assert_eq!(llm_config.model, "custom-model");
        assert_eq!(llm_config.api_key, "sk_custom");
    }

    #[test]
    fn test_config_serialization() {
        let config = Config {
            llm: LlmConfig {
                endpoint: "http://test:8000".to_string(),
                model: "test-model".to_string(),
                api_key: "test-key".to_string(),
            },
        };

        let toml_str = toml::to_string(&config).expect("Should serialize");
        assert!(toml_str.contains("http://test:8000"));
        assert!(toml_str.contains("test-model"));
    }
}
