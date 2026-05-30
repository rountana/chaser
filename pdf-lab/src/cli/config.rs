use clap::{Args, Subcommand};

use pdf_core::{config::{ClaudeConfig, LlmBackend}, extraction::{claude, gemini, ollama}};

#[derive(Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommand,
}

#[derive(Subcommand)]
pub enum ConfigCommand {
    Set(SetArgs),
    Get,
    Test(TestArgs),
}

#[derive(Args)]
pub struct SetArgs {
    #[arg(long, help = "Anthropic API key")]
    pub api_key: Option<String>,

    #[arg(long, help = "Model to use (claude-haiku-4-5-20251001)")]
    pub model: Option<String>,

    #[arg(long, help = "Custom base URL for API proxy (leave unset for api.anthropic.com)")]
    pub base_url: Option<String>,

    #[arg(long, help = "Default source directory scanned for PDFs/images when no paths given")]
    pub source_dir: Option<String>,

    #[arg(long, help = "Default output directory for extracted .md files")]
    pub index_dir: Option<String>,

    #[arg(long, help = "Ollama base URL (default: http://localhost:11434)")]
    pub ollama_url: Option<String>,

    #[arg(long, help = "Ollama vision model name (default: llama3.2-vision)")]
    pub ollama_model: Option<String>,

    #[arg(long, help = "Google Gemini API key")]
    pub gemini_api_key: Option<String>,

    #[arg(long, help = "Gemini model name (default: gemini-2.5-flash)")]
    pub gemini_model: Option<String>,

    #[arg(long, help = "LLM backend to use for extraction and search: claude, gemini, ollama")]
    pub backend: Option<String>,
}

#[derive(Args)]
pub struct TestArgs {
    #[arg(long, help = "Test local Ollama connection instead of Claude")]
    pub local: bool,

    #[arg(long, help = "Test Google Gemini connection")]
    pub gemini: bool,
}

pub async fn run(args: ConfigArgs) -> anyhow::Result<()> {
    match args.command {
        ConfigCommand::Set(set_args) => run_set(set_args),
        ConfigCommand::Get => run_get(),
        ConfigCommand::Test(test_args) => run_test(test_args).await,
    }
}

fn run_set(args: SetArgs) -> anyhow::Result<()> {
    let mut config = ClaudeConfig::load()?;

    if let Some(key) = args.api_key {
        config.api_key = key;
    }
    if let Some(model) = args.model {
        let valid_models = ["claude-haiku-4-5-20251001"];
        if !valid_models.contains(&model.as_str()) {
            anyhow::bail!("Invalid model. Choose from: {}", valid_models.join(", "));
        }
        config.model = model;
    }
    if let Some(url) = args.base_url {
        config.base_url = if url.is_empty() { None } else { Some(url) };
    }
    if let Some(dir) = args.source_dir {
        config.source_dir = if dir.is_empty() { None } else { Some(dir) };
    }
    if let Some(dir) = args.index_dir {
        config.index_dir = if dir.is_empty() { None } else { Some(dir) };
    }
    if let Some(url) = args.ollama_url {
        config.ollama_url = if url.is_empty() { None } else { Some(url) };
    }
    if let Some(model) = args.ollama_model {
        config.ollama_model = if model.is_empty() { None } else { Some(model) };
    }
    if let Some(key) = args.gemini_api_key {
        config.gemini_api_key = if key.is_empty() { None } else { Some(key) };
    }
    if let Some(model) = args.gemini_model {
        config.gemini_model = if model.is_empty() { None } else { Some(model) };
    }
    if let Some(b) = args.backend {
        config.backend = match b.to_lowercase().as_str() {
            "gemini" => LlmBackend::Gemini,
            "ollama" => LlmBackend::Ollama,
            "claude" => LlmBackend::Claude,
            other => anyhow::bail!("Invalid backend '{other}'. Choose from: claude, gemini, ollama"),
        };
    }

    config.save()?;
    println!("Config saved to {}", ClaudeConfig::config_path().display());
    Ok(())
}

fn run_get() -> anyhow::Result<()> {
    let config = ClaudeConfig::load()?;
    println!("Config file: {}", ClaudeConfig::config_path().display());
    println!("  model:       {}", config.model);
    let masked = if config.api_key.len() > 8 {
        format!("{}...{}", &config.api_key[..4], &config.api_key[config.api_key.len() - 4..])
    } else if config.api_key.is_empty() {
        "(not set)".to_string()
    } else {
        "****".to_string()
    };
    println!("  api_key:     {masked}");
    println!(
        "  base_url:    {}",
        config.base_url.as_deref().unwrap_or("https://api.anthropic.com")
    );
    println!(
        "  source_dir:  {}",
        config.source_dir.as_deref().unwrap_or("(not set)")
    );
    println!(
        "  index_dir: {}",
        config.index_dir.as_deref().unwrap_or("./outputs (default)")
    );
    println!("  backend:     {}", config.backend);
    println!();
    println!("Local (Ollama):");
    println!("  ollama_url:   {}", config.ollama_base());
    println!("  ollama_model: {}", config.resolved_ollama_model());

    println!();
    println!("Gemini:");
    let gemini_key_display = match &config.gemini_api_key {
        Some(k) if k.len() > 8 => format!("{}...{}", &k[..4], &k[k.len()-4..]),
        Some(_) => "****".to_string(),
        None => "(not set)".to_string(),
    };
    println!("  gemini_api_key: {gemini_key_display}");
    println!("  gemini_model:   {}", config.resolved_gemini_model());
    Ok(())
}

async fn run_test(test_args: TestArgs) -> anyhow::Result<()> {
    let config = ClaudeConfig::load()?;
    if test_args.gemini {
        let key = config.gemini_api_key.as_deref().unwrap_or("");
        if key.is_empty() {
            anyhow::bail!("No Gemini API key set. Run: pdf-lab config set --gemini-api-key YOUR_KEY");
        }
        println!("Testing connection to Gemini with model {}...", config.resolved_gemini_model());
        match gemini::test_connection(&config).await {
            Ok(elapsed) => println!("  OK — latency: {}ms", elapsed.as_millis()),
            Err(e) => anyhow::bail!("Connection failed: {e}"),
        }
    } else if test_args.local {
        println!("Testing Ollama at {} with model {}...", config.ollama_base(), config.resolved_ollama_model());
        match ollama::test_connection(&config).await {
            Ok(elapsed) => println!("  OK — latency: {}ms", elapsed.as_millis()),
            Err(e) => anyhow::bail!("Connection failed: {e}"),
        }
    } else {
        if config.api_key.is_empty() {
            anyhow::bail!("No API key set. Run: pdf-lab config set --api-key YOUR_KEY\n\
                           (or use `config test --local` to test your Ollama connection)");
        }
        println!("Testing connection to {} with model {}...", config.api_base(), config.model);
        match claude::test_connection(&config).await {
            Ok(elapsed) => println!("  OK — latency: {}ms", elapsed.as_millis()),
            Err(e) => anyhow::bail!("Connection failed: {e}"),
        }
    }
    Ok(())
}
