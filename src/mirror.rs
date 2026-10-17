//! The desktop field's mirror (`web/kb.rs`): what to type on the desktop
//! so its field follows an edit iOS made to the keyboard's textarea.
//!
//! The textarea holds a copy of the desktop field around its caret (the
//! text its app reports, or what was typed here), and iOS edits it
//! natively — so its autocorrect, predictions, dictation and the space
//! bar's trackpad work, which they cannot on a field whose every edit is
//! prevented. Each change is diffed against the copy and replayed as plain
//! keys: arrows to the change's end, Backspace over what it replaced, the
//! new characters. A caret move alone is arrows.
//!
//! Positions are in chars of the textarea's whole value, and `desk` is
//! where the desktop's caret is in it. Pure, and compiled on every target so
//! its tests run natively.

use crate::predict::Edit;

/// What one change types, and where it leaves things.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub keys: Vec<Edit>,
    /// The textarea's value once typed: the new value, but with each
    /// character the keys could not type as typed (`fold`) — a curly quote
    /// as a straight one, an emoji not at all — so the copy stays the
    /// desktop's.
    pub text: String,
    /// The desktop's caret after the keys, in chars of `text`.
    pub desk: usize,
}

/// A character as the keys type it: itself where a US layout has it, the
/// nearest that does for iOS's smart punctuation, nothing otherwise.
pub fn fold(c: char) -> &'static str {
    match c {
        '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' | '\u{2032}' => "'",
        '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{201f}' | '\u{2033}' => "\"",
        '\u{2013}' | '\u{2014}' => "-",
        '\u{2026}' => "...",
        '\u{a0}' => " ",
        _ => {
            const ASCII: &str = " !\"#$%&'()*+,-./0123456789:;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~\n\t";
            ASCII.find(c).map_or("", |i| &ASCII[i..i + 1])
        }
    }
}

/// The arrows from `from` to `to`.
pub fn moves(from: usize, to: usize) -> Vec<Edit> {
    let key = if to > from { Edit::Right } else { Edit::Left };
    vec![key; from.abs_diff(to)]
}

/// The keys that turn the desktop's field from `old` into `new`.
///
/// `desk` is the desktop's caret in `old`, and `sel` its selection if it
/// has one (collapsed first, with a Right, as a field collapses one to its
/// end); `caret` is where the textarea's caret is after the change, `None`
/// for a selection (which the desktop is not given: a later edit is
/// replayed over it from the caret instead).
///
/// The change is the span between the two values' common prefix and common
/// suffix. The prefix stops at the desktop's caret: an edit happens there
/// (typing, Backspace, autocorrect's word before it), and in a run of one
/// letter the diff could put it anywhere along the run. Any prefix and
/// suffix that hold give a right result; these give the fewest keys.
pub fn plan(old: &str, desk: usize, sel: Option<(usize, usize)>, new: &str, caret: Option<usize>) -> Plan {
    let o: Vec<char> = old.chars().collect();
    let n: Vec<char> = new.chars().collect();
    let mut keys = Vec::new();
    let mut desk = desk.min(o.len());
    if let Some((lo, hi)) = sel.filter(|(lo, hi)| lo != hi) {
        keys.push(Edit::Right);
        desk = lo.max(hi).min(o.len());
    }
    let mut p = 0;
    while p < o.len() && p < n.len() && o[p] == n[p] && p < desk {
        p += 1;
    }
    let mut q = 0;
    while q < o.len() - p && q < n.len() - p && o[o.len() - 1 - q] == n[n.len() - 1 - q] {
        q += 1;
    }
    let (e, ne) = (o.len() - q, n.len() - q);
    keys.extend(moves(desk, e));
    keys.extend(std::iter::repeat_n(Edit::Backspace, e - p));
    let typed: String = n[p..ne].iter().map(|&c| fold(c)).collect();
    keys.extend(typed.chars().map(Edit::Insert));
    let typed_n = typed.chars().count();
    let text: String = o[..p].iter().chain(typed.chars().collect::<Vec<_>>().iter()).chain(o[e..].iter()).collect();
    let after = p + typed_n;
    // The textarea's caret, in the typed text: past the change it keeps its
    // place from the end, in it it goes to the change's end.
    let to = match caret {
        Some(c) if c >= ne => after + (c - ne),
        Some(c) if c <= p => c,
        Some(_) => after,
        None => after,
    };
    keys.extend(moves(after, to));
    Plan { keys, text, desk: to }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Edit::*;

    fn ins(s: &str) -> Vec<Edit> {
        s.chars().map(Insert).collect()
    }

    #[test]
    fn typing_and_deleting_at_the_caret() {
        // A letter at the end, and in a run of the same letter, types just it.
        assert_eq!(plan("ab", 2, None, "abc", Some(3)), Plan { keys: ins("c"), text: "abc".into(), desk: 3 });
        assert_eq!(plan("aa", 1, None, "aaa", Some(2)).keys, ins("a"));
        // Backspace, and a word's worth.
        assert_eq!(plan("abc", 3, None, "ab", Some(2)).keys, [Backspace]);
        assert_eq!(plan("hi there", 8, None, "hi ", Some(3)).keys, vec![Backspace; 5]);
        // Forward delete: to the change's end, then back over it.
        assert_eq!(plan("abc", 1, None, "ac", Some(1)).keys, [Right, Backspace]);
    }

    #[test]
    fn autocorrect_replaces_the_word_before_the_caret() {
        // "teh" + space, corrected: back over what differs, the rest typed.
        let p = plan("I saw teh", 9, None, "I saw the ", Some(10));
        assert_eq!(p.keys, [vec![Backspace, Backspace], ins("he ")].concat());
        assert_eq!((p.text.as_str(), p.desk), ("I saw the ", 10));
    }

    #[test]
    fn the_caret_moving_is_arrows() {
        assert_eq!(plan("hello", 5, None, "hello", Some(2)).keys, [Left, Left, Left]);
        assert_eq!(plan("hello", 1, None, "hello", Some(4)).keys, [Right, Right, Right]);
        // A selection gives the desktop nothing.
        assert_eq!(plan("hello", 5, None, "hello", None).keys, []);
    }

    #[test]
    fn a_selection_typed_over_is_replayed_from_the_caret() {
        // The textarea's "llo" selected and replaced by "p"; the desktop's
        // caret stayed at 5, so it backs over "llo" and types "p".
        let p = plan("hello", 5, None, "hep", Some(3));
        assert_eq!(p.keys, [vec![Backspace; 3], ins("p")].concat());
        // A desktop selection is collapsed first, to its end.
        let p = plan("hello", 1, Some((1, 5)), "h", Some(1));
        assert_eq!(p.keys, [vec![Right], vec![Backspace; 4]].concat());
    }

    #[test]
    fn what_the_keys_cannot_type_is_folded_or_left_out() {
        let p = plan("dont", 3, None, "don\u{2019}t", Some(4));
        assert_eq!(p.keys, ins("'"));
        assert_eq!((p.text.as_str(), p.desk), ("don't", 4));
        let p = plan("a", 1, None, "a\u{1f600}b", Some(3));
        assert_eq!(p.keys, ins("b"));
        assert_eq!((p.text.as_str(), p.desk), ("ab", 2));
    }

    #[test]
    fn replaying_a_plan_gives_the_new_text() {
        // Every plan, applied as a field would, ends at its text and caret.
        let cases = [
            ("the quick fox", 13, "the quick brown fox", Some(19)),
            ("the quick fox", 9, "the quick, sly fox", Some(14)),
            ("abc def", 7, "abc", Some(3)),
            ("x\ny", 3, "x\nyz\n", Some(5)),
            ("same", 2, "same", Some(0)),
        ];
        for (old, desk, new, caret) in cases {
            let p = plan(old, desk, None, new, caret);
            let at = old.char_indices().nth(desk).map_or(old.len(), |(b, _)| b);
            let mut f = (old.to_string(), at, at);
            for k in &p.keys {
                crate::predict::apply(&mut f, *k);
            }
            let desk_b = p.text.char_indices().nth(p.desk).map_or(p.text.len(), |(b, _)| b);
            assert_eq!((f.0.as_str(), f.1), (p.text.as_str(), desk_b), "{old:?} -> {new:?}");
        }
    }
}
