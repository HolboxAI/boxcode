use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub llm: LlmConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LlmConfig {
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub api_key: String,
}

impl Config {
    pub fn load() -> Result<Self, Box<dyn std::error::Error>> {
        let config_path = Self::config_path();

        let mut config = if config_path.exists() {
            let contents = std::fs::read_to_string(&config_path)?;
            toml::from_str::<Config>(&contents).map_err(|e| {
                format!("{} is not valid TOML: {e}", config_path.display())
            })?
        } else {
            Self::default()
        };

        // Environment variables win over the file, so exporting a key works even
        // when a stale config.toml is on disk.
        if let Some(v) = env_var("TUISAMPLE_ENDPOINT") {
            config.llm.endpoint = v;
        }
        if let Some(v) = env_var("TUISAMPLE_MODEL") {
            config.llm.model = v;
        }
        if let Some(v) = env_var("TUISAMPLE_API_KEY") {
            config.llm.api_key = v;
        }

        config.normalize();
        Ok(config)
    }

    /// Trim stray whitespace. A trailing newline in an API key (very easy to get
    /// from `export KEY=$(cat file)`) produces an invalid Authorization header.
    fn normalize(&mut self) {
        self.llm.endpoint = self.llm.endpoint.trim().trim_end_matches('/').to_string();
        self.llm.model = self.llm.model.trim().to_string();
        self.llm.api_key = self.llm.api_key.trim().to_string();

        let d = LlmConfig::default();
        if self.llm.endpoint.is_empty() {
            self.llm.endpoint = d.endpoint;
        }
        if self.llm.model.is_empty() {
            self.llm.model = d.model;
        }
    }

    /// Human-readable reasons the app cannot talk to an endpoint yet, shown on
    /// the welcome screen rather than failing silently on the first prompt.
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if self.llm.api_key.is_empty() {
            warnings.push(
                "No API key set. Export TUISAMPLE_API_KEY or add api_key to ~/.tuisample-code/config.toml."
                    .to_string(),
            );
        }
        if !self.llm.endpoint.starts_with("http://") && !self.llm.endpoint.starts_with("https://") {
            warnings.push(format!(
                "Endpoint '{}' has no http:// or https:// scheme.",
                self.llm.endpoint
            ));
        }
        warnings
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

    pub fn config_path() -> PathBuf {
        let home = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        home.join(".tuisample-code").join("config.toml")
    }
}

fn env_var(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ => None,
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            llm: LlmConfig::default(),
        }
    }
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://localhost:8000".to_string(),
            model: "gpt-3.5-turbo".to_string(),
            api_key: String::new(),
        }
    }
}
