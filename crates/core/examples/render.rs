//! Dumps how a message body parses, for looking at real mail that renders wrong.
//!
//! Usage: cargo run -p spool-core --example render -- <file.html> [--plain]

use spool_core::html::{Block, Source, body_to_blocks};

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: render <file.html> [--plain]");
    let source = if args.any(|a| a == "--plain") { Source::Plain } else { Source::Rich };

    let text = std::fs::read_to_string(&path).expect("could not read the file");
    let (plain, html) = if source == Source::Plain {
        (text.as_str(), "")
    } else {
        ("", text.as_str())
    };

    let blocks = body_to_blocks(plain, html, source);
    println!("{} blocks from {} bytes\n", blocks.len(), text.len());

    let mut counts = std::collections::BTreeMap::new();
    for block in &blocks {
        let kind = match block {
            Block::Paragraph(_) => "paragraph",
            Block::Heading { .. } => "heading",
            Block::Quote { .. } => "quote",
            Block::ListItem { .. } => "list-item",
            Block::Code(_) => "code",
            Block::Image(_) => "image",
            Block::Table(_) => "table",
            Block::Rule => "rule",
        };
        *counts.entry(kind).or_insert(0usize) += 1;
    }
    for (kind, n) in &counts {
        println!("  {n:>4}  {kind}");
    }

    println!("\n--- first 25 blocks ---");
    for (ix, block) in blocks.iter().take(25).enumerate() {
        let text = block.text();
        let shown: String = text.chars().take(64).collect();
        let marks: String = block
            .spans()
            .iter()
            .filter_map(|s| {
                let mut m = String::new();
                if s.style.bold { m.push('B') }
                if s.style.mono { m.push('M') }
                if let Some(c) = s.style.color { m.push_str(&format!(" #{:02x}{:02x}{:02x}", c.r, c.g, c.b)) }
                if let Some(c) = s.style.background { m.push_str(&format!(" bg#{:02x}{:02x}{:02x}@{}", c.r, c.g, c.b, c.a)) }
                (!m.is_empty()).then_some(m)
            })
            .collect::<Vec<_>>()
            .join(" | ");
        println!("{ix:>3}  {shown:<66} {marks}");
    }
}
