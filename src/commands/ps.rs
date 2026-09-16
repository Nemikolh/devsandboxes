use anyhow::Result;

use crate::render::table;
use crate::runtime::{backend, NAME_PREFIX};

const HEADERS: [&str; 3] = ["NAME", "IMAGE", "STATUS"];

pub fn ps(all: bool) -> Result<()> {
    let mut rows = backend().list(all, NAME_PREFIX)?;
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    let cells: Vec<[String; 3]> = rows.into_iter().map(|r| [r.name, r.image, r.status]).collect();
    print!("{}", table(&HEADERS, &cells));
    Ok(())
}
