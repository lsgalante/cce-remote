//! The echo strip's prediction: show a keystroke in the desktop field's text
//! the moment it is typed on the phone, rather than after the round trip —
//! the key to the desktop, the app's repaint, its new surrounding text
//! relayed back (`tt`) — which made the strip lag behind the thumbs.
//!
//! The page keeps the last text the desktop confirmed and the edits typed
//! since; it shows the confirmed text with those edits applied. When the
//! desktop reports again, [`reconcile`] finds how many of the pending edits
//! the report already reflects — the shortest prefix that, applied to the
//! old text, gives the new one — and drops them. A report no prefix explains
//! (the app rewrote the text, refused a character, completed a word) is
//! simply the truth: the page drops its predictions and shows it.
//!
//! An edit it cannot model — Enter, Up and Down, a key with a modifier held
//! — is [`Edit::Opaque`]: the prediction stops there, and the report after it
//! is taken as the truth.
//!
//! Pure, and compiled on every target so its tests run natively.

/// The field as the desktop reports it: text, then the caret and the
/// selection's other end as byte offsets (equal with no selection).
pub type Field = (String, usize, usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    Insert(char),
    Backspace,
    Delete,
    Left,
    Right,
    /// Anything the prediction cannot follow.
    Opaque,
}

/// Apply `edit` to `field` as a one-line text field would. False for
/// [`Edit::Opaque`], which changes nothing.
pub fn apply(field: &mut Field, edit: Edit) -> bool {
    let (text, cursor, anchor) = field;
    let (lo, hi) = ((*cursor).min(*anchor), (*cursor).max(*anchor));
    let selected = lo != hi;
    let prev = |t: &str, i: usize| t[..i].chars().next_back().map_or(i, |c| i - c.len_utf8());
    let next = |t: &str, i: usize| t[i..].chars().next().map_or(i, |c| i + c.len_utf8());
    let at = match edit {
        Edit::Opaque => return false,
        Edit::Insert(c) => {
            text.replace_range(lo..hi, &c.to_string());
            lo + c.len_utf8()
        }
        Edit::Backspace if selected => {
            text.replace_range(lo..hi, "");
            lo
        }
        Edit::Backspace => {
            let from = prev(text, *cursor);
            text.replace_range(from..*cursor, "");
            from
        }
        Edit::Delete if selected => {
            text.replace_range(lo..hi, "");
            lo
        }
        Edit::Delete => {
            let to = next(text, *cursor);
            text.replace_range(*cursor..to, "");
            *cursor
        }
        Edit::Left if selected => lo,
        Edit::Left => prev(text, *cursor),
        Edit::Right if selected => hi,
        Edit::Right => next(text, *cursor),
    };
    *cursor = at;
    *anchor = at;
    true
}

/// `base` with every pending edit applied, stopping at the first one that
/// cannot be followed: what the strip shows.
pub fn shown(base: &Field, pending: &[Edit]) -> Field {
    let mut f = base.clone();
    for &e in pending {
        if !apply(&mut f, e) {
            break;
        }
    }
    f
}

/// How many of `pending` the new report `now` already reflects, applied to
/// the last report `base`: the shortest prefix that turns `base` into `now`
/// (text and caret). `None` when none does.
pub fn reconcile(base: &Field, pending: &[Edit], now: &Field) -> Option<usize> {
    let same = |a: &Field| a.0 == now.0 && a.1 == now.1;
    let mut f = base.clone();
    if same(&f) {
        return Some(0);
    }
    for (i, &e) in pending.iter().enumerate() {
        if !apply(&mut f, e) {
            return None;
        }
        if same(&f) {
            return Some(i + 1);
        }
    }
    None
}

/// The pending edits a new report leaves: those it has not yet reflected.
/// A report no prefix explains drops them all, except that one arriving
/// after an [`Edit::Opaque`] is taken to be that edit's result, so only the
/// edits through it go.
pub fn remaining(base: &Field, pending: &[Edit], now: &Field) -> Vec<Edit> {
    match reconcile(base, pending, now) {
        Some(k) => pending[k..].to_vec(),
        None => match pending.iter().position(|&e| e == Edit::Opaque) {
            Some(i) => pending[i + 1..].to_vec(),
            None => Vec::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(text: &str, cursor: usize) -> Field {
        (text.to_string(), cursor, cursor)
    }

    #[test]
    fn edits_apply_as_a_field_would() {
        let mut x = f("hé", 3);
        apply(&mut x, Edit::Insert('!'));
        assert_eq!(x, f("hé!", 4));
        apply(&mut x, Edit::Left);
        apply(&mut x, Edit::Backspace); // removes the two-byte é
        assert_eq!(x, f("h!", 1));
        apply(&mut x, Edit::Delete);
        assert_eq!(x, f("h", 1));
        apply(&mut x, Edit::Right); // at the end: stays
        assert_eq!(x, f("h", 1));
        // A selection is replaced, or deleted, or collapsed.
        let mut s: Field = ("hello".into(), 5, 0);
        apply(&mut s, Edit::Insert('x'));
        assert_eq!(s, f("x", 1));
        let mut s: Field = ("hello".into(), 1, 3);
        apply(&mut s, Edit::Backspace);
        assert_eq!(s, f("hlo", 1));
        let mut s: Field = ("hello".into(), 1, 3);
        apply(&mut s, Edit::Right);
        assert_eq!(s, f("hello", 3));
        assert!(!apply(&mut f("a", 1), Edit::Opaque));
    }

    #[test]
    fn the_strip_shows_the_report_plus_what_is_still_in_flight() {
        let base = f("hi", 2);
        let typed = [Edit::Insert(' '), Edit::Insert('t'), Edit::Insert('o')];
        assert_eq!(shown(&base, &typed), f("hi to", 5));
        // Prediction stops at what it cannot follow.
        let with_enter = [Edit::Insert('!'), Edit::Opaque, Edit::Insert('x')];
        assert_eq!(shown(&base, &with_enter), f("hi!", 3));
    }

    #[test]
    fn a_report_retires_the_edits_it_reflects() {
        let base = f("hi", 2);
        let typed = vec![Edit::Insert(' '), Edit::Insert('t'), Edit::Insert('o')];
        // The desktop has caught up with two of three.
        assert_eq!(remaining(&base, &typed, &f("hi t", 4)), vec![Edit::Insert('o')]);
        // Nothing new yet: all still pending.
        assert_eq!(remaining(&base, &typed, &f("hi", 2)), typed);
        // All of them.
        assert!(remaining(&base, &typed, &f("hi to", 5)).is_empty());
    }

    #[test]
    fn a_report_nothing_explains_is_the_truth() {
        let base = f("hi", 2);
        // The app capitalised, or completed, or refused: drop the guesses.
        let typed = vec![Edit::Insert('x'), Edit::Insert('y')];
        assert!(remaining(&base, &typed, &f("Hix", 3)).is_empty());
        // After an edit the prediction could not follow, the report is that
        // edit's result: only what was typed after it stays pending.
        let typed = vec![Edit::Insert('!'), Edit::Opaque, Edit::Insert('x')];
        assert_eq!(remaining(&base, &typed, &f("", 0)), vec![Edit::Insert('x')]);
    }
}
