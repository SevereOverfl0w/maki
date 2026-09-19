//! One key identity and one spelling for it.
//!
//! A keypress a plugin can see is a [`Key`]: a normalized [`KeyCode`] plus
//! [`KeyModifiers`]. There is one constructor from a terminal event and one
//! from notation, both of which normalize, and nothing else builds one. The
//! two directions read the same const tables, so a key is added in one line in
//! one place and `Key::parse(k.notation()) == Ok(k)` holds for every `Key`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// The names both directions read, canonical spelling only. Adding a key is
/// one line here; an extra spelling users may type is one line in
/// [`NAME_ALIASES`].
const NAMED_KEYS: &[(&str, KeyCode)] = &[
    ("CR", KeyCode::Enter),
    ("Esc", KeyCode::Esc),
    ("BS", KeyCode::Backspace),
    ("Del", KeyCode::Delete),
    ("Tab", KeyCode::Tab),
    ("Space", KeyCode::Char(' ')),
    ("Up", KeyCode::Up),
    ("Down", KeyCode::Down),
    ("Left", KeyCode::Left),
    ("Right", KeyCode::Right),
    ("Home", KeyCode::Home),
    ("End", KeyCode::End),
    ("PageUp", KeyCode::PageUp),
    ("PageDown", KeyCode::PageDown),
    ("Insert", KeyCode::Insert),
];

/// Spellings accepted on the way in and never printed on the way out. Each
/// target is a name in [`NAMED_KEYS`], which a test holds to.
const NAME_ALIASES: &[(&str, &str)] = &[
    ("Enter", "CR"),
    ("Return", "CR"),
    ("Escape", "Esc"),
    ("Backspace", "BS"),
    ("Delete", "Del"),
];

/// Modifier prefixes, canonical spelling first for each bit. [`Key::notation`]
/// walks this in order and takes the first prefix per bit, so the `C-` `M-`
/// `S-` order a canonical string is printed in is derived from this table
/// rather than written a second time.
const MODIFIERS: &[(&str, KeyModifiers)] = &[
    ("C-", KeyModifiers::CONTROL),
    ("M-", KeyModifiers::ALT),
    ("S-", KeyModifiers::SHIFT),
    ("ctrl-", KeyModifiers::CONTROL),
    ("alt-", KeyModifiers::ALT),
    ("a-", KeyModifiers::ALT),
    ("shift-", KeyModifiers::SHIFT),
];

/// The modifier bits notation can spell. A terminal reporting `SUPER`,
/// `HYPER` or `META` hands us a keypress no notation names, and a key that
/// cannot be named is not a [`Key`].
const NAMEABLE_MODIFIERS: KeyModifiers = KeyModifiers::CONTROL
    .union(KeyModifiers::ALT)
    .union(KeyModifiers::SHIFT);

/// Kitty reports past F12, and `<F13>` is valid vim notation.
const MAX_FUNCTION_KEY: u8 = 24;

/// The keys the host resolves before it looks at a binding at all: quitting
/// and suspending have to work whatever a handler is doing. Binding one would
/// publish a mapping that can never fire, so it is refused instead.
///
/// One list for all three sides of that promise: the host weighs a key
/// against [`is_reserved`] before it dispatches, `maki.keymap.set` refuses the
/// same entries, and so does the `keys` an unfocused window claims, so no side
/// can grow a key the others do not know about.
pub const RESERVED_KEYS: [Key; 2] = [
    Key {
        code: KeyCode::Char('c'),
        modifiers: KeyModifiers::CONTROL,
    },
    Key {
        code: KeyCode::Char('z'),
        modifiers: KeyModifiers::CONTROL,
    },
];

/// Every notation [`Key::parse`] accepts in canonical spelling, which is what
/// a misspelling is measured against. Derived from the same tables the two
/// directions read, so a key added to [`NAMED_KEYS`] is a key the lint can
/// suggest without a second list to keep in step.
pub(crate) fn candidate_notations() -> Vec<String> {
    let names: Vec<String> = NAMED_KEYS
        .iter()
        .map(|(name, _)| (*name).to_owned())
        .chain((1..=MAX_FUNCTION_KEY).map(|n| format!("F{n}")))
        .chain(('a'..='z').map(String::from))
        .chain(('0'..='9').map(String::from))
        .collect();
    let prefixes = ["", "C-", "M-", "S-", "C-M-", "C-S-", "M-S-", "C-M-S-"];
    prefixes
        .iter()
        .flat_map(|prefix| names.iter().map(move |name| format!("<{prefix}{name}>")))
        .collect()
}

/// A keypress as plugins see it.
///
/// Fields rather than a wrapped [`KeyEvent`]: crossterm's equality includes
/// `kind` and `state`, and a key identity that compares those is a bug waiting
/// for a terminal that sets them.
///
/// `Copy` despite the single-primitive-field guideline: this is a fixed-size
/// value type mirroring `KeyEvent`, which is itself `Copy`, and it is compared
/// and passed once per keystroke.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Key {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl Key {
    /// The identity of {event}, or `None` when no notation names it: media
    /// keys, bare modifiers, and anything carrying a modifier bit notation
    /// cannot spell. Deciding nameability once, here at the boundary, is what
    /// keeps every caller from checking for an empty string.
    pub fn from_event(event: KeyEvent) -> Option<Self> {
        if !NAMEABLE_MODIFIERS.contains(event.modifiers) {
            return None;
        }
        let key = Self::new(event.code, event.modifiers);
        name_of(key.code).is_some().then_some(key)
    }

    /// Reads vim notation: `<C-n>`, `<S-Tab>`, `<Space>`, `<F13>`, `a`.
    pub fn parse(lhs: &str) -> Result<Self, String> {
        let s = lhs.trim();
        if s.is_empty() {
            return Err("empty key notation".into());
        }

        if s.len() > 2 && s.starts_with('<') && s.ends_with('>') {
            let (modifiers, name) = strip_modifiers(&s[1..s.len() - 1]);
            return Ok(Self::new(code_of(name)?, modifiers));
        }

        let mut chars = s.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => Ok(Self::new(KeyCode::Char(c), KeyModifiers::NONE)),
            _ => Err(format!("invalid key notation: {s}")),
        }
    }

    /// The one spelling of this key, which [`Key::parse`] reads back into it.
    pub fn notation(&self) -> String {
        let name = name_of(self.code).unwrap_or_default();
        if self.modifiers.is_empty() && name.chars().count() == 1 {
            return name;
        }
        let mut out = String::from("<");
        for (i, (prefix, bits)) in MODIFIERS.iter().enumerate() {
            let already_spelled = MODIFIERS[..i].iter().any(|(_, seen)| seen == bits);
            if self.modifiers.contains(*bits) && !already_spelled {
                out.push_str(prefix);
            }
        }
        out.push_str(&name);
        out.push('>');
        out
    }

    pub fn code(&self) -> KeyCode {
        self.code
    }

    pub fn modifiers(&self) -> KeyModifiers {
        self.modifiers
    }

    /// Whether the host answers this key itself, whatever any plugin bound.
    pub fn is_reserved(&self) -> bool {
        RESERVED_KEYS.contains(self)
    }

    fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        let (code, modifiers) = normalize(code, modifiers);
        Self { code, modifiers }
    }
}

impl From<Key> for KeyEvent {
    /// The host keeps [`KeyEvent`] internally and matches on codes, so its
    /// event loop normalizes its own view through here rather than converting
    /// the whole UI. `kind` and `state` are not part of a key's identity and
    /// nothing in maki matches on them.
    fn from(key: Key) -> Self {
        KeyEvent::new(key.code, key.modifiers)
    }
}

/// Whether the host answers {key} itself, whatever any plugin bound. A key no
/// notation names is not one the host reserves.
pub fn is_reserved(key: KeyEvent) -> bool {
    Key::from_event(key).is_some_and(|k| k.is_reserved())
}

/// The three places terminals disagree with themselves, settled one way.
/// Idempotent, which is what makes it safe to call at every layer.
///
/// 1. `(Tab, SHIFT)` and `(BackTab, _)` both become `(BackTab, SHIFT)`. Maki
///    pushes the kitty disambiguation flags, so a terminal that speaks the
///    protocol reports Shift+Tab as `Tab + SHIFT` and every other one reports
///    `CSI Z`, i.e. `BackTab`. One form either way.
/// 2. `CONTROL` plus an ASCII letter lowercases it: `<C-N>` and `<C-n>` are
///    one key, the vim rule. `<C-S-n>` stays distinct where a terminal can
///    report it.
/// 3. `SHIFT` plus an uppercase char drops `SHIFT`, because the shift is
///    already in the letter. Digits and punctuation keep it, so shift+digit
///    bindings go on working.
fn normalize(code: KeyCode, modifiers: KeyModifiers) -> (KeyCode, KeyModifiers) {
    let mut modifiers = modifiers;
    let mut code = match code {
        KeyCode::Tab if modifiers.contains(KeyModifiers::SHIFT) => KeyCode::BackTab,
        other => other,
    };
    if code == KeyCode::BackTab {
        modifiers |= KeyModifiers::SHIFT;
    }
    if let KeyCode::Char(c) = code
        && modifiers.contains(KeyModifiers::CONTROL)
        && c.is_ascii_alphabetic()
    {
        code = KeyCode::Char(c.to_ascii_lowercase());
    }
    if let KeyCode::Char(c) = code
        && modifiers.contains(KeyModifiers::SHIFT)
        && c.is_uppercase()
    {
        modifiers.remove(KeyModifiers::SHIFT);
    }
    (code, modifiers)
}

/// The canonical name of {code} without its modifiers, or `None` when
/// notation has no word for it. `BackTab` is spelled `Tab`, the `SHIFT`
/// normalization put on it supplying the `S-`.
fn name_of(code: KeyCode) -> Option<String> {
    let code = if code == KeyCode::BackTab {
        KeyCode::Tab
    } else {
        code
    };
    if let Some((name, _)) = NAMED_KEYS.iter().find(|(_, c)| *c == code) {
        return Some((*name).to_owned());
    }
    match code {
        KeyCode::Char(c) => Some(c.to_string()),
        KeyCode::F(n @ 1..=MAX_FUNCTION_KEY) => Some(format!("F{n}")),
        _ => None,
    }
}

fn strip_modifiers(inner: &str) -> (KeyModifiers, &str) {
    let mut modifiers = KeyModifiers::NONE;
    let mut rest = inner;
    'strip: loop {
        for (prefix, bits) in MODIFIERS {
            if rest.len() > prefix.len()
                && rest
                    .get(..prefix.len())
                    .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
            {
                modifiers |= *bits;
                rest = &rest[prefix.len()..];
                continue 'strip;
            }
        }
        return (modifiers, rest);
    }
}

fn code_of(name: &str) -> Result<KeyCode, String> {
    let canonical = NAME_ALIASES
        .iter()
        .find(|(alias, _)| alias.eq_ignore_ascii_case(name))
        .map_or(name, |(_, target)| target);
    if let Some((_, code)) = NAMED_KEYS
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(canonical))
    {
        return Ok(*code);
    }

    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return Ok(KeyCode::Char(c));
    }

    if let Some(number) = name.strip_prefix(['f', 'F'])
        && let Ok(n) = number.parse::<u8>()
    {
        return match n {
            1..=MAX_FUNCTION_KEY => Ok(KeyCode::F(n)),
            _ => Err(format!("function key out of range: {name}")),
        };
    }

    Err(format!("unknown key: {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};
    use test_case::test_case;

    const CONTROL: KeyModifiers = KeyModifiers::CONTROL;
    const ALT: KeyModifiers = KeyModifiers::ALT;
    const SHIFT: KeyModifiers = KeyModifiers::SHIFT;
    const NONE: KeyModifiers = KeyModifiers::NONE;

    fn event(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    /// The tables are the spec, so every entry in them is a test rather than
    /// a line somebody remembered to add below.
    #[test]
    fn every_named_key_round_trips() {
        for (name, code) in NAMED_KEYS {
            let spelling = format!("<{name}>");
            let key = Key::parse(&spelling).unwrap();
            assert_eq!(key.code(), *code, "{spelling} parses to its table entry");
            assert_eq!(key.notation(), spelling, "and prints back as itself");
        }
    }

    #[test]
    fn every_alias_prints_as_its_canonical_name() {
        for (alias, target) in NAME_ALIASES {
            let canonical = NAMED_KEYS
                .iter()
                .find(|(name, _)| name == target)
                .unwrap_or_else(|| panic!("alias {alias} targets unknown name {target}"));
            let key = Key::parse(&format!("<{alias}>")).unwrap();
            assert_eq!(key.code(), canonical.1);
            assert_eq!(key.notation(), format!("<{target}>"));
        }
    }

    #[test]
    fn every_modifier_alias_parses_to_its_bit() {
        for (prefix, bits) in MODIFIERS {
            let key = Key::parse(&format!("<{prefix}q>")).unwrap();
            assert_eq!(key.modifiers(), *bits, "{prefix} is {bits:?}");
        }
    }

    #[test_case(KeyCode::Tab, SHIFT ; "shift_tab_on_a_kitty_terminal")]
    #[test_case(KeyCode::BackTab, NONE ; "shift_tab_everywhere_else")]
    #[test_case(KeyCode::BackTab, SHIFT ; "back_tab_already_carrying_shift")]
    #[test_case(KeyCode::Char('N'), CONTROL ; "ctrl_upper_letter")]
    #[test_case(KeyCode::Char('n'), CONTROL.union(SHIFT) ; "ctrl_shift_letter")]
    #[test_case(KeyCode::Char('A'), SHIFT ; "shift_upper_letter")]
    #[test_case(KeyCode::Char('1'), SHIFT ; "shift_digit")]
    #[test_case(KeyCode::Char(' '), CONTROL ; "ctrl_space")]
    #[test_case(KeyCode::F(24), ALT ; "alt_f24")]
    #[test_case(KeyCode::Enter, NONE ; "plain_enter")]
    fn normalization_is_idempotent_and_notation_round_trips(code: KeyCode, mods: KeyModifiers) {
        let (once, once_mods) = normalize(code, mods);
        assert_eq!(
            normalize(once, once_mods),
            (once, once_mods),
            "applying it twice changes nothing"
        );

        let key = Key::from_event(event(code, mods)).unwrap();
        assert_eq!(Key::parse(&key.notation()), Ok(key), "{}", key.notation());
    }

    #[test_case(KeyCode::Tab, SHIFT, "<S-Tab>" ; "shift_tab_from_kitty")]
    #[test_case(KeyCode::BackTab, NONE, "<S-Tab>" ; "shift_tab_from_csi_z")]
    #[test_case(KeyCode::Char('N'), CONTROL, "<C-n>" ; "ctrl_letter_is_lowercased")]
    #[test_case(KeyCode::Char(' '), CONTROL, "<C-Space>" ; "ctrl_space")]
    #[test_case(KeyCode::Char(' '), NONE, "<Space>" ; "space")]
    #[test_case(KeyCode::Char('A'), SHIFT, "A" ; "shift_is_already_in_the_letter")]
    #[test_case(KeyCode::Char('1'), SHIFT, "<S-1>" ; "shift_digit_keeps_its_bit")]
    #[test_case(KeyCode::Char('x'), ALT, "<M-x>" ; "alt_is_printed_as_m")]
    #[test_case(KeyCode::Home, ALT, "<M-Home>" ; "alt_home")]
    #[test_case(KeyCode::Char('d'), CONTROL, "<C-d>" ; "ctrl_d")]
    #[test_case(KeyCode::F(5), NONE, "<F5>" ; "f5")]
    #[test_case(KeyCode::F(13), NONE, "<F13>" ; "f13")]
    #[test_case(KeyCode::Char('a'), NONE, "a" ; "plain_char")]
    #[test_case(KeyCode::Enter, NONE, "<CR>" ; "enter")]
    #[test_case(KeyCode::Esc, NONE, "<Esc>" ; "escape")]
    #[test_case(KeyCode::Char('n'), CONTROL.union(ALT), "<C-M-n>" ; "modifier_order_is_derived")]
    fn notation_cases(code: KeyCode, mods: KeyModifiers, expected: &str) {
        let key = Key::from_event(event(code, mods)).expect("nameable");
        assert_eq!(key.notation(), expected);
    }

    #[test_case("<Enter>", KeyCode::Enter, NONE ; "enter_alias")]
    #[test_case("<Shift-Tab>", KeyCode::BackTab, SHIFT ; "long_shift_tab")]
    #[test_case("<Ctrl-x>", KeyCode::Char('x'), CONTROL ; "long_ctrl")]
    #[test_case("<Alt-j>", KeyCode::Char('j'), ALT ; "long_alt")]
    #[test_case("<A-x>", KeyCode::Char('x'), ALT ; "short_alt")]
    #[test_case("<C-S-a>", KeyCode::Char('a'), CONTROL.union(SHIFT) ; "ctrl_shift")]
    #[test_case("<C-T>", KeyCode::Char('t'), CONTROL ; "ctrl_upper_is_one_key_with_ctrl_lower")]
    #[test_case("<", KeyCode::Char('<'), NONE ; "bare_angle_bracket")]
    fn parse_cases(input: &str, code: KeyCode, mods: KeyModifiers) {
        let key = Key::parse(input).unwrap();
        assert_eq!(key.code(), code);
        assert_eq!(key.modifiers(), mods);
    }

    #[test_case("" ; "empty")]
    #[test_case("<>" ; "empty_brackets")]
    #[test_case("<F0>" ; "function_key_zero")]
    #[test_case("<F25>" ; "function_key_past_the_end")]
    #[test_case("<lt>" ; "vim_escape_we_do_not_need")]
    #[test_case("abc" ; "a_word")]
    fn parse_refuses(input: &str) {
        assert!(Key::parse(input).is_err(), "{input} must not parse");
    }

    /// Nameability is decided here so no caller has to test for an empty
    /// string, and a key nothing can spell reaches no plugin.
    #[test_case(KeyCode::Media(crossterm::event::MediaKeyCode::Play), NONE ; "media")]
    #[test_case(KeyCode::Modifier(crossterm::event::ModifierKeyCode::LeftShift), NONE ; "bare_modifier")]
    #[test_case(KeyCode::CapsLock, NONE ; "caps_lock")]
    #[test_case(KeyCode::Null, NONE ; "null")]
    #[test_case(KeyCode::Char('a'), KeyModifiers::SUPER ; "modifier_notation_cannot_spell")]
    fn from_event_refuses_what_notation_cannot_name(code: KeyCode, mods: KeyModifiers) {
        assert_eq!(Key::from_event(event(code, mods)), None);
    }

    /// A reserved key no terminal can deliver would quietly reserve nothing.
    #[test]
    fn reserved_keys_are_normalized_and_nameable() {
        for key in RESERVED_KEYS {
            assert_eq!(Key::parse(&key.notation()), Ok(key));
            assert!(key.is_reserved());
            assert!(is_reserved(event(key.code(), key.modifiers())));
        }
    }
}
