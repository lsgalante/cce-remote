//! The Agent view's text wrapping (`web/agent.rs`): pure, so tested
//! natively. The page's face is monospace, so a line is a count of
//! characters.

/// `text` wrapped to `cols` characters a line, at spaces where it can be;
/// its own line breaks kept.
pub fn wrap(text: &str, cols: usize) -> Vec<String> {
    let cols = cols.max(8);
    let mut out = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split(' ') {
            let wlen = word.chars().count();
            let llen = line.chars().count();
            if llen > 0 && llen + 1 + wlen > cols {
                out.push(std::mem::take(&mut line));
            } else if llen > 0 {
                line.push(' ');
            }
            // A word longer than a line is cut.
            let mut w = word.to_string();
            while w.chars().count() > cols {
                let head: String = w.chars().take(cols).collect();
                if !line.is_empty() {
                    out.push(std::mem::take(&mut line));
                }
                out.push(head);
                w = w.chars().skip(cols).collect();
            }
            line.push_str(&w);
        }
        out.push(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_at_spaces_and_keeps_breaks() {
        assert_eq!(wrap("the quick brown fox", 9), ["the quick", "brown fox"]);
        assert_eq!(wrap("a\n\nb", 20), ["a", "", "b"]);
        assert_eq!(wrap("abcdefghijklmnopqrstuvwxyz", 10), ["abcdefghij", "klmnopqrst", "uvwxyz"]);
    }
}
