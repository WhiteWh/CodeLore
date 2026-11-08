//! A file cut into the pieces a search returns: at definitions where the file
//! has them (a function with the comment above it, a class), at fixed windows
//! where it has none. A piece is what an agent reads instead of the file.
use super::lang::Symbol;

/// A piece shorter than this joins the next one.
const MIN_LINES: usize = 12;
/// A piece longer than this is cut again, at inner definitions or in windows.
const MAX_LINES: usize = 120;
/// The window where a file has no definition to cut at.
const WINDOW: usize = 60;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    /// 1-based, inclusive.
    pub start: usize,
    pub end: usize,
    /// The definitions that start inside it, in order.
    pub symbols: Vec<String>,
}

/// Lines that belong to the definition below them.
fn leading(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("///")
        || trimmed.starts_with("//")
        || trimmed.starts_with("#[")
        || trimmed.starts_with("/**")
        || trimmed.starts_with("* ")
        || trimmed == "*"
        || trimmed.starts_with("*/")
        || trimmed.starts_with('@')
        || trimmed.starts_with("--")
        || trimmed.starts_with("[") && trimmed.ends_with(']')
}

/// The line a definition's piece starts at: above its comments and attributes.
fn start_of(lines: &[&str], line: usize, floor: usize) -> usize {
    let mut start = line;
    while start > floor + 1 && leading(lines[start - 2]) {
        start -= 1;
    }
    start
}

fn windows(start: usize, end: usize, size: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut at = start;
    while at <= end {
        let stop = (at + size - 1).min(end);
        out.push((at, stop));
        at = stop + 1;
    }
    out
}

/// Cuts a file of `lines` lines with these definitions.
pub fn chunk(text: &str, symbols: &[Symbol]) -> Vec<Chunk> {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    if total == 0 {
        return Vec::new();
    }
    let ranges = if symbols.is_empty() {
        windows(1, total, WINDOW)
    } else {
        cut(&lines, symbols, 1, total, 0)
    };
    let mut out: Vec<Chunk> = Vec::new();
    for (start, end) in ranges {
        let names: Vec<String> = symbols.iter().filter(|s| s.line >= start && s.line <= end).map(|s| s.name.clone()).collect();
        out.push(Chunk { start, end, symbols: names });
    }
    // Trailing blank-only pieces say nothing.
    out.retain(|chunk| lines[chunk.start - 1..chunk.end].iter().any(|line| !line.trim().is_empty()));
    out
}

fn cut(lines: &[&str], symbols: &[Symbol], from: usize, to: usize, depth: usize) -> Vec<(usize, usize)> {
    let inside: Vec<&Symbol> = symbols.iter().filter(|s| s.line > from && s.line <= to || s.line == from && depth == 0).collect();
    if inside.is_empty() || depth > 3 {
        return windows(from, to, MAX_LINES.min(WINDOW + 20));
    }
    let base = inside.iter().map(|s| s.indent).min().unwrap_or(0);
    let mut starts: Vec<usize> = inside.iter().filter(|s| s.indent <= base).map(|s| start_of(lines, s.line, from - 1)).collect();
    starts.dedup();
    if starts.first().copied() != Some(from) {
        starts.insert(0, from);
    }
    // Pieces between consecutive starts, the small ones joined to the next.
    let mut raw: Vec<(usize, usize)> = Vec::new();
    for (i, &start) in starts.iter().enumerate() {
        let end = starts.get(i + 1).map_or(to, |next| next - 1);
        if end >= start {
            raw.push((start, end));
        }
    }
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in raw {
        match merged.last_mut() {
            Some(last) if last.1 - last.0 + 1 < MIN_LINES && end - last.0 + 1 <= MAX_LINES => last.1 = end,
            _ => merged.push((start, end)),
        }
    }
    let mut out = Vec::new();
    for (start, end) in merged {
        if end - start + 1 > MAX_LINES {
            // An impl or a class: cut again at the definitions inside it.
            let deeper: Vec<Symbol> = symbols.iter().filter(|s| s.line > start && s.line <= end && s.indent > base).cloned().collect();
            if deeper.is_empty() {
                out.extend(windows(start, end, WINDOW + 20));
            } else {
                out.extend(cut(lines, &deeper, start, end, depth + 1));
            }
        } else {
            out.push((start, end));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atlas::lang::{symbols, Lang};

    #[test]
    fn pieces_start_at_definitions_with_their_comments() {
        let mut text = String::new();
        for n in 0..3 {
            text.push_str(&format!("/// The function number {n}.\nfn f{n}() {{\n"));
            for _ in 0..15 {
                text.push_str("    work();\n");
            }
            text.push_str("}\n");
        }
        let found = symbols(Lang::Rust, &text);
        let chunks = chunk(&text, &found);
        assert_eq!(chunks.len(), 3);
        assert_eq!((chunks[1].start, chunks[1].symbols.clone()), (19, vec!["f1".to_owned()]));
    }

    #[test]
    fn a_file_without_definitions_is_cut_in_windows() {
        let text = "x\n".repeat(130);
        let chunks = chunk(&text, &[]);
        assert_eq!(chunks.iter().map(|c| (c.start, c.end)).collect::<Vec<_>>(), [(1, 60), (61, 120), (121, 130)]);
    }
}
