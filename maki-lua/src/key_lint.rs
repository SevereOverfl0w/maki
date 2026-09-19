//! Key spellings a plugin writes as a bare Lua string, checked at load.
//!
//! Every other place a plugin names a key goes through [`Key::parse`] and
//! fails loudly: `maki.keymap.set` and the `keys` an `open_win` claims are
//! both parsed before they take effect. The one spelling nothing can check is
//! the literal in `ev.key == "<Esc>"`, because that comparison happens inside
//! Lua and the host never sees the string. A typo there compares false
//! forever, and a spelling from before keys were unified compares false
//! forever too.
//!
//! So the source is read instead. Only string literals are looked at, via the
//! Lua grammar rather than a regex, so a key named in a comment is not a
//! finding. Two rules, each chosen to fire only on a string that is a key and
//! wrong.
//!
//! **A misspelling**, meaning a bracketed spelling that does not parse but
//! that one edit away would. `"<Escc>"` is a finding, `"<path>"` is not,
//! because nothing one edit from `<path>` is a key. That is what keeps
//! placeholder strings quiet. This rule has no end date: a typo in a key
//! comparison is silent for as long as plugins compare strings.
//!
//! **A legacy spelling**, meaning one maki itself handed plugins before keys
//! were unified. This rule is temporary, and [`legacy`] is written to be
//! deleted whole: the module, its two calls in [`check`], the cases naming
//! [`LEGACY_HINT`], and the migration paragraph under `maki.keymap`. Nothing
//! else reads it. It exists because the unification changed `win:recv`'s
//! `ev.key` from `"esc"` to `"<Esc>"`, and a bare string compare is the one
//! break no shim can cover: a plugin holds the old spelling, the host never
//! sees the comparison, and no value equals both. Once plugins written before
//! that change are gone, so is the reason for this.
//!
//! Findings are warnings, never a refused load: this reads code it did not
//! parse for meaning, and a false positive must cost a line of output rather
//! than a plugin that will not start.

use std::sync::LazyLock;

use tree_sitter::{Node, Parser};

use crate::key::{Key, candidate_notations};

/// One edit, because two admits real words. `<Escc>` is one from `<Esc>`;
/// nothing a plugin writes for another reason is one edit from a key.
const MAX_EDIT_DISTANCE: usize = 1;

const STRING_CONTENT_KIND: &str = "string_content";
pub(crate) const LEGACY_HINT: &str = "is the old spelling of";
pub(crate) const MISSPELLED_HINT: &str = "is not a key; did you mean";

static CANDIDATES: LazyLock<Vec<String>> = LazyLock::new(candidate_notations);

/// Every key spelling in {source} that is wrong, as a line naming {chunk} and
/// the line it sits on.
///
/// Source that does not parse yields nothing rather than an error: a syntax
/// error is the load's own to report, and with a better message than this
/// could give.
pub(crate) fn lint(chunk: &str, source: &str) -> Vec<String> {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_lua::LANGUAGE.into())
        .is_err()
    {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };

    let mut findings = Vec::new();
    let mut cursor = tree.walk();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == STRING_CONTENT_KIND {
            if let Ok(text) = node.utf8_text(source.as_bytes())
                && let Some(problem) = check(text)
            {
                let line = node.start_position().row + 1;
                findings.push(format!("{chunk}:{line}: {problem}"));
            }
            continue;
        }
        stack.extend(node.children(&mut cursor).collect::<Vec<Node<'_>>>());
    }
    findings.sort();
    findings
}

/// What is wrong with {text} as a key spelling, or `None` when it is a key, or
/// not meant to be one.
fn check(text: &str) -> Option<String> {
    if Key::parse(text).is_ok() {
        return None;
    }
    if let Some(canonical) = legacy::notation(text) {
        return Some(format!("{text:?} {LEGACY_HINT} {canonical:?}"));
    }
    let bracketed = text.starts_with('<') && text.ends_with('>') && text.len() > 2;
    let suggestion = bracketed.then(|| nearest(text)).flatten()?;
    Some(format!("{text:?} {MISSPELLED_HINT} {suggestion:?}?"))
}

/// The spellings maki handed plugins before keys were unified.
///
/// Temporary, and separated so it can go in one piece: delete this module,
/// its call in [`check`], and the cases naming [`LEGACY_HINT`]. Nothing else
/// reads it.
mod legacy {
    use crate::key::Key;

    /// The legacy modifier spellings, and the notation prefix each becomes.
    const MODIFIERS: &[(&str, &str)] = &[("ctrl+", "C-"), ("alt+", "M-"), ("shift+", "S-")];

    /// Legacy key names, and the canonical name each becomes. Read for the
    /// tail of a `ctrl+`/`alt+`/`shift+` spelling, where the modifier already
    /// proves the string is a key.
    const NAMES: &[(&str, &str)] = &[
        ("esc", "Esc"),
        ("enter", "CR"),
        ("tab", "Tab"),
        ("backspace", "BS"),
        ("delete", "Del"),
        ("space", "Space"),
        ("up", "Up"),
        ("down", "Down"),
        ("left", "Left"),
        ("right", "Right"),
        ("home", "Home"),
        ("end", "End"),
        ("pageup", "PageUp"),
        ("pagedown", "PageDown"),
        ("insert", "Insert"),
    ];

    /// The legacy names worth flagging with no modifier in front of them.
    /// Every other entry in [`NAMES`] is a word a plugin writes for reasons
    /// that have nothing to do with keys: this repo alone has `split = "left"`
    /// and `anchor` values that read the same. A lint that cries on those is a
    /// lint people learn to scroll past.
    const BARE_NAMES: &[&str] = &["esc", "enter", "backspace", "pageup", "pagedown"];

    /// The canonical notation {text} used to be spelled as, or `None` when it
    /// was never a key spelling. The answer is built and then parsed, so a
    /// suggestion is always a string the host really accepts.
    ///
    /// Case-sensitive, and that is the rule that makes this usable: the
    /// spelling maki used to hand a plugin was always lowercase, so only a
    /// lowercase string can be a comparison left over from it. `"Ctrl+N"` and
    /// `"Enter"` are what a plugin writes in a footer for the user to read,
    /// and every bundled plugin has a row of them.
    pub(super) fn notation(text: &str) -> Option<String> {
        let mut rest = text;
        let mut prefix = String::new();
        'strip: loop {
            for (legacy, canonical) in MODIFIERS {
                if let Some(tail) = rest.strip_prefix(legacy) {
                    prefix.push_str(canonical);
                    rest = tail;
                    continue 'strip;
                }
            }
            break;
        }
        if prefix.is_empty() && !BARE_NAMES.contains(&rest) {
            return None;
        }
        let name = NAMES
            .iter()
            .find(|(legacy, _)| *legacy == rest)
            .map(|(_, canonical)| (*canonical).to_owned())
            .or_else(|| (rest.chars().count() == 1).then(|| rest.to_owned()))?;
        Key::parse(&format!("<{prefix}{name}>"))
            .ok()
            .map(|key| key.notation())
    }
}

/// The key {text} was probably meant to be, if exactly one is close enough.
/// Ties answer `None`: a lint that guesses between `<C-a>` and `<C-b>` is
/// worse than one that says nothing.
fn nearest(text: &str) -> Option<String> {
    let mut best: Option<&String> = None;
    for candidate in CANDIDATES.iter() {
        if edit_distance(text, candidate) > MAX_EDIT_DISTANCE {
            continue;
        }
        if best.is_some() {
            return None;
        }
        best = Some(candidate);
    }
    best.cloned()
}

/// Levenshtein distance, stopped once it cannot come in under
/// [`MAX_EDIT_DISTANCE`], which is almost always on the length check.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > MAX_EDIT_DISTANCE {
        return MAX_EDIT_DISTANCE + 1;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut row = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            row[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(row[j] + 1);
        }
        std::mem::swap(&mut prev, &mut row);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::{LEGACY_HINT, MISSPELLED_HINT, check, lint};
    use test_case::test_case;

    const CHUNK: &str = "init.lua";

    #[test_case("<CR>" ; "canonical")]
    #[test_case("<Enter>" ; "alias")]
    #[test_case("<esc>" ; "lowercased_name")]
    #[test_case("a" ; "plain_char")]
    #[test_case("<C-n>" ; "ctrl_letter")]
    fn a_real_key_is_not_a_finding(text: &str) {
        assert_eq!(check(text), None);
    }

    /// The strings that made a bracketed-literal rule look unusable: a lint
    /// firing on these is one nobody reads.
    #[test_case("<path>" ; "placeholder")]
    #[test_case("<div>" ; "markup")]
    #[test_case("left" ; "split_direction")]
    #[test_case("end" ; "keyword_like")]
    #[test_case("insert" ; "verb")]
    #[test_case("north" ; "unrelated")]
    #[test_case("" ; "empty")]
    #[test_case("Enter" ; "footer_label")]
    #[test_case("Ctrl+N" ; "footer_label_with_modifier")]
    #[test_case("Esc" ; "capitalized_label")]
    fn a_string_that_is_not_a_key_is_not_a_finding(text: &str) {
        assert_eq!(check(text), None);
    }

    /// Goes with [`super::legacy`]: delete this case when that module goes.
    #[test_case("ctrl+n", "<C-n>" ; "ctrl_letter")]
    #[test_case("alt+x", "<M-x>" ; "alt_letter")]
    #[test_case("shift+tab", "<S-Tab>" ; "shift_tab")]
    #[test_case("esc", "<Esc>" ; "bare_esc")]
    #[test_case("enter", "<CR>" ; "bare_enter")]
    #[test_case("pagedown", "<PageDown>" ; "bare_pagedown")]
    fn a_legacy_spelling_names_what_replaced_it(text: &str, canonical: &str) {
        let finding = check(text).expect("legacy spelling is a finding");
        assert!(
            finding.contains(LEGACY_HINT) && finding.contains(canonical),
            "{finding}"
        );
    }

    #[test_case("<Escc>", "<Esc>" ; "extra_letter")]
    #[test_case("<C-nn>", "<C-n>" ; "doubled_modifier_target")]
    #[test_case("<Tabb>", "<Tab>" ; "trailing_letter")]
    fn a_near_miss_suggests_the_key_it_missed(text: &str, suggestion: &str) {
        let finding = check(text).expect("near miss is a finding");
        assert!(
            finding.contains(MISSPELLED_HINT) && finding.contains(suggestion),
            "{finding}"
        );
    }

    /// One edit from both `<C-a>` and `<C-b>`, so there is nothing honest to
    /// suggest.
    #[test]
    fn an_ambiguous_near_miss_suggests_nothing() {
        assert_eq!(check("<C->"), None);
    }

    #[test]
    fn a_key_named_in_a_comment_is_not_a_finding() {
        let source = "-- press esc to close, or ctrl+n\nlocal x = 1\n";

        assert!(lint(CHUNK, source).is_empty());
    }

    #[test]
    fn a_finding_names_the_chunk_and_line() {
        let source = "local a = 1\nif ev.key == \"ctrl+n\" then end\n";

        let findings = lint(CHUNK, source);

        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].starts_with("init.lua:2: "), "{findings:?}");
    }

    #[test]
    fn source_that_does_not_parse_yields_nothing() {
        assert!(lint(CHUNK, "local = = =").is_empty());
    }
}
