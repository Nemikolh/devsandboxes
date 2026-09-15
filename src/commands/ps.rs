use anyhow::Result;

use crate::runtime::{backend, ContainerRow, NAME_PREFIX};

const HEADERS: [&str; 3] = ["NAME", "IMAGE", "STATUS"];

pub fn ps(all: bool) -> Result<()> {
    let mut rows = backend().list(all, NAME_PREFIX)?;
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    print!("{}", render(&rows));
    Ok(())
}

/// Fixed-width table, two-space gutter, no trailing padding on the last column.
fn render(rows: &[ContainerRow]) -> String {
    let cells: Vec<[&str; 3]> = rows
        .iter()
        .map(|r| [r.name.as_str(), r.image.as_str(), r.status.as_str()])
        .collect();
    let mut widths = HEADERS.map(str::len);
    for row in &cells {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    let mut out = String::new();
    for row in std::iter::once(&HEADERS).chain(&cells) {
        for (i, cell) in row.iter().enumerate() {
            if i > 0 {
                out.push_str("  ");
            }
            out.push_str(cell);
            if i < 2 {
                out.extend(std::iter::repeat(' ').take(widths[i] - cell.len()));
            }
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_aligned_table() {
        let rows = vec![
            ContainerRow {
                name: "devsandbox-repo".into(),
                image: "node:22".into(),
                status: "Up 3 minutes".into(),
                ..Default::default()
            },
            ContainerRow {
                name: "devsandbox-x".into(),
                image: "alpine".into(),
                status: "running".into(),
                ..Default::default()
            },
        ];
        assert_eq!(
            render(&rows),
            "NAME             IMAGE    STATUS\n\
             devsandbox-repo  node:22  Up 3 minutes\n\
             devsandbox-x     alpine   running\n"
        );
    }
}
