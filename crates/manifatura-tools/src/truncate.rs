//! Output limits shared by the tools (pi's defaults).

/// Lines a tool's output keeps at most.
pub const MAX_LINES: usize = 2000;
/// Bytes a tool's output keeps at most.
pub const MAX_BYTES: usize = 50 * 1024;
/// Characters one line keeps before [`clip_line`] cuts it.
pub const MAX_LINE_CHARS: usize = 2000;

/// The lines of a text that were kept.
#[derive(Clone, Debug)]
pub struct Cut {
    /// The kept lines, joined with newlines.
    pub text: String,
    /// Lines kept.
    pub lines: usize,
    /// Whether anything before them was left out.
    pub truncated: bool,
}

/// Keeps the last lines that fit, for command output where the end matters most.
pub fn tail(text: &str, max_lines: usize, max_bytes: usize) -> Cut {
    tail_of(text, false, max_lines, max_bytes)
}

/// [`tail`] of the end of a longer text (`cut_before`): its first line may be partial, so it
/// is left out.
pub fn tail_of(text: &str, cut_before: bool, max_lines: usize, max_bytes: usize) -> Cut {
    let mut all = text.lines();
    if cut_before && !text.starts_with('\n') {
        all.next();
    }
    let mut bytes = 0;
    let mut kept = Vec::new();
    let mut truncated = cut_before;
    let mut all = all.rev();
    for line in all.by_ref() {
        let line = clip_line(line);
        if kept.len() == max_lines || (bytes + line.len() + 1 > max_bytes && !kept.is_empty()) {
            truncated = true;
            break;
        }
        bytes += line.len() + 1;
        kept.push(line);
    }
    kept.reverse();
    Cut {
        lines: kept.len(),
        text: kept.join("\n"),
        truncated,
    }
}

/// The last `budget` bytes of a file in `env`, as lines within `max_lines` and `budget`, and
/// the file's size. A file that cannot be read is empty.
pub async fn tail_file(
    env: &dyn manifatura_agent::Env,
    path: &std::path::Path,
    max_lines: usize,
    budget: usize,
) -> (Cut, u64) {
    let (bytes, len) = env.read_tail(path, budget as u64).await.unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    (
        tail_of(&text, len > bytes.len() as u64, max_lines, budget),
        len,
    )
}

/// A size for people: `512 B`, `48 KB`, `3.2 MB`.
pub fn size(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{} KB", bytes / 1024),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
    }
}

/// Shortens one very long line (minified files, base64) so it cannot eat the whole budget.
pub fn clip_line(line: &str) -> String {
    match line.char_indices().nth(MAX_LINE_CHARS) {
        Some((i, _)) => format!("{}… [line truncated]", &line[..i]),
        None => line.to_string(),
    }
}

/// True when the bytes look binary (a NUL in the first 8 KB).
pub fn is_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(8192)].contains(&0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_the_end_within_limits() {
        let text: String = (1..=10).map(|i| format!("line {i}\n")).collect();
        let c = tail(&text, 3, 1000);
        assert_eq!(c.text, "line 8\nline 9\nline 10");
        assert!(c.truncated);
        let c = tail(&text, 100, 15);
        assert_eq!(c.text, "line 9\nline 10");
        assert!(!tail(&text, 10, 1000).truncated);
        // The end of a longer text starts mid-line: that line goes.
        let c = tail_of("ne 9\nline 10\n", true, 10, 1000);
        assert_eq!((c.text.as_str(), c.truncated), ("line 10", true));
        assert_eq!(size(2048), "2 KB");
    }

    #[test]
    fn clip_line_respects_char_boundaries() {
        let long = "é".repeat(MAX_LINE_CHARS + 5);
        assert!(clip_line(&long).ends_with("[line truncated]"));
        assert_eq!(clip_line("short"), "short");
    }
}
