//! The Agent view's text wrapping (`web/agent.rs`): pure, so tested
//! natively. The page's face is monospace, so a line is a count of
//! characters.

/// `text` wrapped to `cols` characters a line, at spaces where it can be;
/// its own line breaks kept.
pub fn wrap(text: &str, cols: usize) -> Vec<String> {
    wrap_ranges(text, cols).into_iter().map(|(a, b)| text[a..b].to_string()).collect()
}

/// [`wrap`]'s lines as byte ranges of `text`, so a caret's offset finds
/// its line and column. Each line is a slice of the text: a wrap at a
/// space drops that one space (it is between the two ranges), a word
/// longer than a line is cut where it overflows, and a line break ends a
/// line (the `\n` is in neither).
pub fn wrap_ranges(text: &str, cols: usize) -> Vec<(usize, usize)> {
    let cols = cols.max(8);
    let mut out = Vec::new();
    let mut base = 0;
    for para in text.split('\n') {
        // The line's start, and its length so far in chars.
        let (mut ls, mut llen) = (base, 0usize);
        let mut pos = base;
        for (i, word) in para.split(' ').enumerate() {
            let ws = pos;
            pos += word.len() + 1;
            let wlen = word.chars().count();
            if i > 0 {
                if llen > 0 && llen + 1 + wlen > cols {
                    out.push((ls, ws - 1));
                    ls = ws;
                    llen = 0;
                } else {
                    llen += 1;
                }
            }
            // A word longer than a line is cut.
            let (mut wpos, mut rest) = (ws, word);
            while rest.chars().count() > cols {
                if llen > 0 {
                    out.push((ls, wpos));
                }
                let cut = rest.char_indices().nth(cols).map_or(rest.len(), |(b, _)| b);
                out.push((wpos, wpos + cut));
                wpos += cut;
                rest = &rest[cut..];
                ls = wpos;
                llen = 0;
            }
            llen += rest.chars().count();
        }
        out.push((ls, base + para.len()));
        base += para.len() + 1;
    }
    out
}

/// The line of `lines` (from [`wrap_ranges`]) a caret at byte `at` stands
/// on: the last that starts at or before it, so a caret where a cut word
/// goes on is at the start of the next line, not the end of this one.
pub fn line_of(lines: &[(usize, usize)], at: usize) -> usize {
    lines.iter().rposition(|&(s, _)| s <= at).unwrap_or(0)
}

/// A UTF-16 offset (a textarea's selection) as a byte offset into `text`.
pub fn utf16_to_byte(text: &str, at: u32) -> usize {
    let mut n = 0u32;
    for (b, c) in text.char_indices() {
        if n >= at {
            return b;
        }
        n += c.len_utf16() as u32;
    }
    text.len()
}

/// A byte offset into `text` as a UTF-16 offset.
pub fn byte_to_utf16(text: &str, at: usize) -> u32 {
    text[..at.min(text.len())].encode_utf16().count() as u32
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

    #[test]
    fn ranges_are_slices_a_caret_can_find() {
        let text = "the quick brown fox";
        let lines = wrap_ranges(text, 9);
        assert_eq!(lines, [(0, 9), (10, 19)]);
        // Before the dropped space: the first line's end; after it, the next's start.
        assert_eq!(line_of(&lines, 9), 0);
        assert_eq!(line_of(&lines, 10), 1);
        // A cut word: its boundary is the next line's.
        let cut = wrap_ranges("abcdefghijklmnopqrstuvwxyz", 10);
        assert_eq!(cut, [(0, 10), (10, 20), (20, 26)]);
        assert_eq!(line_of(&cut, 10), 1);
        // A trailing space stays on its line, where the caret after it is.
        assert_eq!(wrap_ranges("hello ", 20), [(0, 6)]);
        // Line breaks end lines and are in none.
        assert_eq!(wrap_ranges("ab\ncd", 20), [(0, 2), (3, 5)]);
        assert_eq!(line_of(&wrap_ranges("ab\ncd", 20), 2), 0);
    }

    #[test]
    fn utf16_offsets_round_trip() {
        let text = "a€😀b";
        for (b, _) in text.char_indices().chain([(text.len(), ' ')]) {
            assert_eq!(utf16_to_byte(text, byte_to_utf16(text, b)), b);
        }
        assert_eq!(utf16_to_byte(text, 99), text.len());
    }
}
