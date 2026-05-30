use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum LlmBackend {
    Claude,
    Gemini,
    Ollama,
}

impl Default for LlmBackend {
    fn default() -> Self {
        LlmBackend::Claude
    }
}

impl std::fmt::Display for LlmBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LlmBackend::Claude => write!(f, "claude"),
            LlmBackend::Gemini => write!(f, "gemini"),
            LlmBackend::Ollama => write!(f, "ollama"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeConfig {
    pub model: String,
    pub api_key: String,
    pub base_url: Option<String>,
    pub source_dir: Option<String>,
    pub index_dir: Option<String>,
    #[serde(default)]
    pub backend: LlmBackend,
    #[serde(default)]
    pub ollama_url: Option<String>,
    #[serde(default)]
    pub ollama_model: Option<String>,
    /// Override path to schema.toml (default: ~/.config/pdf-lab/schema.toml)
    #[serde(default)]
    pub schema_path: Option<String>,
    #[serde(default)]
    pub gemini_api_key: Option<String>,
    #[serde(default)]
    pub gemini_model: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            model: "claude-haiku-4-5-20251001".to_string(),
            api_key: String::new(),
            base_url: None,
            source_dir: None,
            index_dir: None,
            backend: LlmBackend::Claude,
            ollama_url: None,
            ollama_model: None,
            schema_path: None,
            gemini_api_key: None,
            gemini_model: None,
            mode: None,
        }
    }
}

impl ClaudeConfig {
    pub fn load() -> anyhow::Result<Self> {
        let path = Self::config_path();
        let mut config = if path.exists() {
            let data = std::fs::read_to_string(&path)
                .with_context(|| format!("reading config from {}", path.display()))?;
            serde_json::from_str(&data).with_context(|| "parsing config JSON")?
        } else {
            Self::default()
        };

        // Env vars override config file values.
        if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
            if !key.is_empty() { config.api_key = key; }
        }
        if let Ok(model) = std::env::var("PDF_LAB_MODEL") {
            if !model.is_empty() { config.model = model; }
        }
        if let Ok(url) = std::env::var("ANTHROPIC_BASE_URL") {
            if !url.is_empty() { config.base_url = Some(url); }
        }
        if let Ok(dir) = std::env::var("PDF_LAB_SOURCE_DIR") {
            if !dir.is_empty() { config.source_dir = Some(dir); }
        }
        if let Ok(dir) = std::env::var("PDF_LAB_INDEX_DIR") {
            if !dir.is_empty() { config.index_dir = Some(dir); }
        }
        if let Ok(url) = std::env::var("PDF_LAB_OLLAMA_URL") {
            if !url.is_empty() { config.ollama_url = Some(url); }
        }
        if let Ok(model) = std::env::var("PDF_LAB_OLLAMA_MODEL") {
            if !model.is_empty() { config.ollama_model = Some(model); }
        }
        if let Ok(b) = std::env::var("PDF_LAB_BACKEND") {
            config.backend = match b.to_lowercase().as_str() {
                "gemini" => LlmBackend::Gemini,
                "ollama" => LlmBackend::Ollama,
                _ => LlmBackend::Claude,
            };
        }
        if let Ok(key) = std::env::var("GEMINI_API_KEY") {
            if !key.is_empty() { config.gemini_api_key = Some(key); }
        }
        if let Ok(model) = std::env::var("PDF_LAB_GEMINI_MODEL") {
            if !model.is_empty() { config.gemini_model = Some(model); }
        }

        Ok(config)
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, data)?;
        Ok(())
    }

    pub fn config_path() -> PathBuf {
        PathBuf::from("config/pdf-lab/config.json")
    }

    /// Resolved index directory: CLI flag > config/env > "./outputs"
    pub fn resolve_index_dir(&self, flag: Option<PathBuf>) -> PathBuf {
        flag.or_else(|| self.index_dir.as_deref().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("outputs"))
    }

    /// Resolved source directory: CLI flag > config/env > ./source-files (if it exists) > None
    pub fn resolve_source_dir(&self, flag: Option<PathBuf>) -> Option<PathBuf> {
        flag.or_else(|| self.source_dir.as_deref().map(PathBuf::from))
            .or_else(|| {
                let default = PathBuf::from("source-files");
                if default.is_dir() { Some(default) } else { None }
            })
    }

    pub fn api_base(&self) -> &str {
        self.base_url
            .as_deref()
            .unwrap_or("https://api.anthropic.com")
    }

    pub fn ollama_base(&self) -> &str {
        self.ollama_url.as_deref().unwrap_or("http://localhost:11434")
    }

    pub fn resolved_ollama_model(&self) -> &str {
        self.ollama_model.as_deref().unwrap_or("qwen2.5vl:7b")
    }

    pub fn resolved_gemini_model(&self) -> &str {
        self.gemini_model.as_deref().unwrap_or("gemini-2.0-flash")
    }

    /// Returns true when mode is "offline" or unset. Any config without a mode
    /// field defaults to offline so existing installs are not broken.
    pub fn is_offline(&self) -> bool {
        self.mode.as_deref() != Some("online")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_offline_when_mode_absent() {
        let c = ClaudeConfig::default();
        assert!(c.is_offline());
    }

    #[test]
    fn is_offline_when_mode_offline() {
        let c = ClaudeConfig { mode: Some("offline".to_string()), ..ClaudeConfig::default() };
        assert!(c.is_offline());
    }

    #[test]
    fn is_online_when_mode_online() {
        let c = ClaudeConfig { mode: Some("online".to_string()), ..ClaudeConfig::default() };
        assert!(!c.is_offline());
    }

    #[test]
    fn is_offline_when_mode_garbage() {
        // Unknown values default to offline (safe side)
        let c = ClaudeConfig { mode: Some("garbage".to_string()), ..ClaudeConfig::default() };
        assert!(c.is_offline());
    }
}
