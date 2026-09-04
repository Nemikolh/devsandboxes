use std::path::Path;

use anyhow::Result;

use crate::config::Config;

pub fn ls(dir: &Path) -> Result<()> {
    let config = Config::load(dir)?;
    for sandbox in config.resolve_all()? {
        println!(
            "{}\t{}\t{}",
            sandbox.name,
            sandbox.source(),
            sandbox.folder().unwrap_or("-")
        );
    }
    Ok(())
}
