use anyhow::{Context, Result};

use crate::commands::status::Envelope;
use crate::render::table;
use crate::runtime::{backend, NAME_PREFIX};

const HEADERS: [&str; 3] = ["NAME", "IMAGE", "STATUS"];

pub fn ps(all: bool, json: bool) -> Result<()> {
    let mut rows = backend().list(all, NAME_PREFIX)?;
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    if json {
        let out = serde_json::to_string_pretty(&Envelope::new(rows)).context("serialize ps")?;
        println!("{out}");
        return Ok(());
    }
    let cells: Vec<[String; 3]> = rows.into_iter().map(|r| [r.name, r.image, r.status]).collect();
    print!("{}", table(&HEADERS, &cells));
    Ok(())
}
