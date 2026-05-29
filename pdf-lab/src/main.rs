mod cli;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "pdf-lab", about = "Local document extraction and search", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Extract(cli::extract::ExtractCommand),
    Index(cli::index::IndexArgs),
    Search(cli::search::SearchArgs),
    Config(cli::config::ConfigArgs),
    Mcp(cli::mcp::McpArgs),
    Serve(cli::serve::ServeArgs),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok(); // load .env if present; silently ignore if missing
    let cli = Cli::parse();
    match cli.command {
        Commands::Extract(args) => cli::extract::run(args).await,
        Commands::Index(args) => cli::index::run(args),
        Commands::Search(args) => cli::search::run(args).await,
        Commands::Config(args) => cli::config::run(args).await,
        Commands::Mcp(args) => cli::mcp::run(args).await,
        Commands::Serve(args) => cli::serve::run(args).await,
    }
}
