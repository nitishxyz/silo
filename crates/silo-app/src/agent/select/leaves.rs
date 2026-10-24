//! A row's leaves, with the ids `render_block` gives the texts it draws.

use super::{Kind, Leaf, USER};
use crate::agent::markdown::Block;

/// An assistant text's leaves: its blocks (`{key}-{i}`) in reading order.
pub(in crate::agent) fn block_leaves(key: &str, blocks: &[Block]) -> Vec<Leaf> {
    let mut out = Vec::new();
    for (i, b) in blocks.iter().enumerate() {
        push(&format!("{key}-{i}"), b, &mut out);
    }
    out
}

/// A user bubble's one leaf (none for a message of only files).
pub(in crate::agent) fn user_leaf(text: &str) -> Option<Leaf> {
    (!text.is_empty()).then(|| Leaf { id: USER.to_owned(), text: text.to_owned(), kind: Kind::Lines })
}

fn push(id: &str, block: &Block, out: &mut Vec<Leaf>) {
    match block {
        Block::Para(inline) | Block::Heading(_, inline) => out.push(Leaf { id: id.to_owned(), text: inline.text.clone(), kind: Kind::Text }),
        Block::Code { text, .. } => out.push(Leaf { id: id.to_owned(), text: text.clone(), kind: Kind::Code }),
        Block::List { items, .. } => {
            for (i, item) in items.iter().enumerate() {
                for (j, b) in item.blocks.iter().enumerate() {
                    push(&format!("{id}-{i}-{j}"), b, out);
                }
            }
        }
        Block::Quote(blocks) => {
            for (i, b) in blocks.iter().enumerate() {
                push(&format!("{id}-q{i}"), b, out);
            }
        }
        Block::Rule => {}
        // Not drawn until its header row is complete.
        Block::Table { head, .. } if head.is_empty() => {}
        Block::Table { head, rows, .. } => {
            for (r, row) in std::iter::once(head).chain(rows).enumerate() {
                for (c, cell) in row.iter().enumerate() {
                    let kind = Kind::Cell { table: id.to_owned(), row: r };
                    out.push(Leaf { id: format!("{id}-r{r}-c{c}"), text: cell.text.clone(), kind });
                }
            }
        }
    }
}
