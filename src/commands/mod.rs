pub mod exec;
pub mod ls;
pub mod ps;
pub mod rm;
pub mod run;
pub mod services;

use std::io::Write;

use anyhow::{bail, Context, Result};

/// Numbered menu on stderr; returns the selected index.
pub(crate) fn pick(prompt: &str, options: &[&str]) -> Result<usize> {
    eprintln!("{prompt}:");
    for (i, option) in options.iter().enumerate() {
        eprintln!("  {}) {option}", i + 1);
    }
    eprint!("> ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    let choice: usize = answer.trim().parse().context("invalid selection")?;
    if choice == 0 || choice > options.len() {
        bail!("selection out of range");
    }
    Ok(choice - 1)
}
