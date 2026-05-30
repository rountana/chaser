use std::path::PathBuf;

use clap::Args;

#[derive(Args)]
pub struct IndexArgs {
    #[arg(long, help = "Override output directory of .md files to index")]
    pub index_dir: Option<PathBuf>,
}

pub fn run(_args: IndexArgs) -> anyhow::Result<()> {
    println!("Semantic indexing is not yet available (Phase 4).");
    println!("Run `pdf-lab search` to use metadata search on extracted documents.");
    Ok(())
}
