use anyhow::Result;

use crate::docker::{self, NAME_PREFIX};

pub fn ps(all: bool) -> Result<()> {
    let filter = format!("name=^/{NAME_PREFIX}");
    let mut args = vec!["ps", "--filter", &filter];
    if all {
        args.push("--all");
    }
    args.extend(["--format", "table {{.Names}}\t{{.Image}}\t{{.Status}}"]);
    std::process::exit(docker::run_inherit(&args)?);
}
