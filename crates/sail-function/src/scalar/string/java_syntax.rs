//! Translates `java.util.regex` pattern syntax into what `regex` / `fancy-regex` understand, where the two read the same text
//! differently or not at all.
//!
//! - **Flag groups** are parsed, not passed through. Java's `(?U)` is `UNICODE_CHARACTER_CLASS`; in the Rust engines `U` swaps
//!   greediness, so it is never emitted. Leading, inline (`(?i)` mid-pattern), scoped (`(?i:..)`) and negated (`(?-U)`) groups are
//!   tracked per group scope.
//! - **Class escapes** `\d \w \s \b` (and negations, POSIX `\p{Alpha}`) are ASCII-only in Java and Unicode in the Rust engines.
//!   With `(?U)` they are Unicode, which is what the Rust engines do natively.
//! - **`(?i)`** folds ASCII letters only in Java unless `u` or `U` is also set. It is rewritten to explicit `[aA]` alternatives
//!   (also inside classes); with `u`/`U` the Rust `i` flag (Unicode folding) is used.
//! - **`$`** also matches before a final line terminator in Java. The translation of that is a look-around, which would push every
//!   pattern with a `$` onto the slow engine, so [`translate`] reports `has_dollar` and the caller compiles the Java variant lazily,
//!   only for texts that end in a line terminator.
//! - `\h`, `\H`, `\Q..\E` are expanded.
//!
//! Anything else, including invalid patterns, passes through unchanged so it fails the way it always did.

/// Java's non-multiline `$` as an expression the Rust engines read: end of text, or before a final line terminator (`\r\n` counts as
/// one terminator, so there is no match between the `\r` and the `\n`).
const JAVA_DOLLAR: &str = r"(?:\z|(?=\r\n\z)|(?<!\r)(?=\n\z)|(?=[\r\x{85}\x{2028}\x{2029}]\z))";

/// `\h`: horizontal whitespace.
const HSPACE: &str = r"\x20\t\x{A0}\x{1680}\x{180E}\x{2000}-\x{200A}\x{202F}\x{205F}\x{3000}";
/// ASCII `\s`: `[ \t\n\x0B\f\r]`.
const SPACE: &str = r" \t\n\x0B\f\r";

/// Java POSIX classes: name, `regex` ASCII class, Unicode equivalent used under `(?U)`.
const POSIX: &[(&str, &str, &str)] = &[
    ("Lower", "[[:lower:]]", r"\p{Lowercase}"),
    ("Upper", "[[:upper:]]", r"\p{Uppercase}"),
    ("ASCII", "[[:ascii:]]", "[[:ascii:]]"),
    ("Alpha", "[[:alpha:]]", r"\p{Alphabetic}"),
    ("Digit", "[[:digit:]]", r"\p{Nd}"),
    ("Alnum", "[[:alnum:]]", r"[\p{Alphabetic}\p{Nd}]"),
    ("Punct", "[[:punct:]]", r"\p{P}"),
    ("Graph", "[[:graph:]]", r"[^\p{Z}\p{C}]"),
    ("Print", "[[:print:]]", r"[^\p{C}\p{Zl}\p{Zp}]"),
    ("Blank", "[[:blank:]]", r"[\p{Zs}\t]"),
    ("Cntrl", "[[:cntrl:]]", r"\p{Cc}"),
    ("XDigit", "[[:xdigit:]]", "[0-9a-fA-F]"),
    ("Space", "[ \\t\\n\\x0B\\f\\r]", r"\s"),
];

#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Emit the Java `$` expression instead of a plain `$`.
    pub java_dollar: bool,
    /// Rewrite ASCII-only `(?i)` into explicit alternatives (otherwise the Rust `i` flag is used).
    pub ascii_icase_expansion: bool,
}

#[derive(Debug, Clone)]
pub struct Translated {
    pub pattern: String,
    /// The pattern has a `$` outside a class and outside multiline mode.
    pub has_dollar: bool,
    /// A back-reference appears under ASCII-only `(?i)`; the expansion cannot make that compare case-insensitively, so the caller
    /// must translate again with `ascii_icase_expansion: false`.
    pub needs_icase_fallback: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct State {
    icase: bool,
    unicode_case: bool,
    unicode_class: bool,
    multiline: bool,
    /// Whether the Rust engine currently has `i` on at this point of the output.
    rust_icase: bool,
}

impl State {
    fn ascii_icase(&self, o: Options) -> bool {
        self.icase && !self.unicode_case && o.ascii_icase_expansion
    }

    fn wants_rust_icase(&self, o: Options) -> bool {
        self.icase && (self.unicode_case || !o.ascii_icase_expansion)
    }
}

struct Scanner {
    chars: Vec<char>,
    opts: Options,
    out: String,
    has_dollar: bool,
    needs_fallback: bool,
}

pub fn translate(pattern: &str, opts: Options) -> Translated {
    let mut s = Scanner {
        chars: pattern.chars().collect(),
        opts,
        out: String::with_capacity(pattern.len() + 16),
        has_dollar: false,
        needs_fallback: false,
    };
    s.run();
    Translated {
        pattern: s.out,
        has_dollar: s.has_dollar,
        needs_icase_fallback: s.needs_fallback,
    }
}

impl Scanner {
    fn at(&self, i: usize) -> Option<char> {
        self.chars.get(i).copied()
    }

    fn run(&mut self) {
        let mut st = State::default();
        let mut stack: Vec<State> = Vec::new();
        let mut i = 0;
        while i < self.chars.len() {
            let c = self.chars[i];
            match c {
                '\\' => i = self.escape(i, &st, false),
                '[' => i = self.class(i, &st),
                '(' => i = self.group_open(i, &mut st, &mut stack),
                ')' => {
                    if let Some(prev) = stack.pop() {
                        st = prev;
                    }
                    self.out.push(')');
                    i += 1;
                }
                '$' if !st.multiline => {
                    self.has_dollar = true;
                    self.out.push_str(if self.opts.java_dollar {
                        JAVA_DOLLAR
                    } else {
                        "$"
                    });
                    i += 1;
                }
                c if st.ascii_icase(self.opts) && c.is_ascii_alphabetic() => {
                    self.push_both_cases(c);
                    i += 1;
                }
                _ => {
                    self.out.push(c);
                    i += 1;
                }
            }
        }
    }

    fn push_both_cases(&mut self, c: char) {
        self.out.push('[');
        self.out.push(c.to_ascii_lowercase());
        self.out.push(c.to_ascii_uppercase());
        self.out.push(']');
    }

    /// `(`: plain, non-capturing, look-around, named or flag group. Returns the index after the opening.
    fn group_open(&mut self, i: usize, st: &mut State, stack: &mut Vec<State>) -> usize {
        if self.at(i + 1) != Some('?') {
            stack.push(*st);
            self.out.push('(');
            return i + 1;
        }
        match self.at(i + 2) {
            Some(':' | '=' | '!' | '>') => {
                stack.push(*st);
                self.out.push_str("(?");
                self.out.push(self.chars[i + 2]);
                i + 3
            }
            Some('<') => {
                stack.push(*st);
                self.out.push_str("(?<");
                let mut j = i + 3;
                if !matches!(self.at(j), Some('=' | '!')) {
                    // a group name: copied verbatim, never case-expanded
                    while let Some(c) = self.at(j) {
                        self.out.push(c);
                        j += 1;
                        if c == '>' {
                            break;
                        }
                    }
                    return j;
                }
                self.out.push(self.chars[j]);
                j + 1
            }
            _ => self.flag_group(i, st, stack),
        }
    }

    fn flag_group(&mut self, i: usize, st: &mut State, stack: &mut Vec<State>) -> usize {
        let mut j = i + 2;
        let (mut on, mut off) = (String::new(), String::new());
        let mut negative = false;
        while let Some(c) = self.at(j) {
            if c == '-' {
                negative = true;
            } else if c.is_ascii_alphabetic() {
                if negative { &mut off } else { &mut on }.push(c);
            } else {
                break;
            }
            j += 1;
        }
        let terminator = self.at(j);
        if !matches!(terminator, Some(')' | ':')) || (on.is_empty() && off.is_empty()) {
            // not a flag group: let the Rust engine judge it
            stack.push(*st);
            self.out.push('(');
            return i + 1;
        }
        let mut new = *st;
        let (mut on_rust, mut off_rust) = (String::new(), String::new());
        for f in on.chars() {
            match f {
                'i' => new.icase = true,
                'u' => new.unicode_case = true,
                'U' => {
                    new.unicode_class = true;
                    new.unicode_case = true;
                }
                'm' => {
                    new.multiline = true;
                    on_rust.push('m');
                }
                's' | 'x' => on_rust.push(f),
                'd' | 'c' => {}
                other => on_rust.push(other),
            }
        }
        for f in off.chars() {
            match f {
                'i' => new.icase = false,
                'u' => new.unicode_case = new.unicode_class,
                'U' => new.unicode_class = false,
                'm' => {
                    new.multiline = false;
                    off_rust.push('m');
                }
                's' | 'x' => off_rust.push(f),
                'd' | 'c' => {}
                other => off_rust.push(other),
            }
        }
        let want = new.wants_rust_icase(self.opts);
        if want != st.rust_icase {
            if want { &mut on_rust } else { &mut off_rust }.push('i');
        }
        new.rust_icase = want;
        let mut flags = on_rust;
        if !off_rust.is_empty() {
            flags.push('-');
            flags.push_str(&off_rust);
        }
        if terminator == Some(')') {
            *st = new;
            if !flags.is_empty() {
                self.out.push_str(&format!("(?{flags})"));
            }
        } else {
            stack.push(*st);
            *st = new;
            self.out.push_str(&format!("(?{flags}:"));
        }
        j + 1
    }

    /// A character class starting at `[`, nested classes included. Returns the index after its `]`.
    fn class(&mut self, mut i: usize, st: &State) -> usize {
        self.out.push('[');
        i += 1;
        if self.at(i) == Some('^') {
            self.out.push('^');
            i += 1;
        }
        let ascii_icase = st.ascii_icase(self.opts);
        let mut extras: Vec<(char, char)> = Vec::new();
        while let Some(c) = self.at(i) {
            match c {
                ']' => {
                    i += 1;
                    break;
                }
                '[' => i = self.class(i, st),
                '&' if self.at(i + 1) == Some('&') => {
                    self.out.push_str("&&");
                    i += 2;
                }
                '\\' => i = self.escape(i, st, true),
                _ => {
                    let range_end = (self.at(i + 1) == Some('-'))
                        .then(|| self.at(i + 2))
                        .flatten()
                        .filter(|h| !matches!(h, ']' | '[' | '\\'));
                    if let Some(h) = range_end {
                        self.out.push(c);
                        self.out.push('-');
                        self.out.push(h);
                        if ascii_icase {
                            extras.push((c, h));
                        }
                        i += 3;
                    } else {
                        self.out.push(c);
                        if ascii_icase {
                            extras.push((c, c));
                        }
                        i += 1;
                    }
                }
            }
        }
        for (lo, hi) in extras {
            self.push_case_counterparts(lo, hi);
        }
        self.out.push(']');
        i
    }

    /// For a class range `lo-hi`, the other case of its ASCII letters.
    fn push_case_counterparts(&mut self, lo: char, hi: char) {
        let (lo, hi) = (lo as u32, hi as u32);
        for (from, to, delta) in [('a', 'z', -32i32), ('A', 'Z', 32i32)] {
            let (a, b) = (lo.max(from as u32), hi.min(to as u32));
            if a <= b {
                let shift = |x: u32| char::from_u32((x as i32 + delta) as u32).unwrap_or(' ');
                self.out.push(shift(a));
                if a != b {
                    self.out.push('-');
                    self.out.push(shift(b));
                }
            }
        }
    }

    /// An escape starting at the backslash at `i`. Returns the index after the escape.
    fn escape(&mut self, i: usize, st: &State, in_class: bool) -> usize {
        let Some(n) = self.at(i + 1) else {
            self.out.push('\\');
            return i + 1;
        };
        let uni = st.unicode_class;
        let mut next = i + 2;
        match n {
            'd' => self.out.push_str(match (uni, in_class) {
                (true, _) => r"\d",
                (false, true) => "0-9",
                (false, false) => "[0-9]",
            }),
            'D' => self.out.push_str(if uni { r"\D" } else { "[^0-9]" }),
            'w' => self.out.push_str(match (uni, in_class) {
                (true, _) => r"\w",
                (false, true) => "a-zA-Z0-9_",
                (false, false) => "[a-zA-Z0-9_]",
            }),
            'W' => self.out.push_str(if uni { r"\W" } else { "[^a-zA-Z0-9_]" }),
            's' => match (uni, in_class) {
                (true, _) => self.out.push_str(r"\s"),
                (false, true) => self.out.push_str(SPACE),
                (false, false) => self.out.push_str(&format!("[{SPACE}]")),
            },
            'S' => self.out.push_str(&if uni {
                r"\S".to_string()
            } else {
                format!("[^{SPACE}]")
            }),
            'h' if in_class => self.out.push_str(HSPACE),
            'h' => self.out.push_str(&format!("[{HSPACE}]")),
            'H' => self.out.push_str(&format!("[^{HSPACE}]")),
            'b' if !in_class => self.out.push_str(if uni { r"\b" } else { r"(?-u:\b)" }),
            'B' if !in_class => self.out.push_str(if uni { r"\B" } else { r"(?-u:\B)" }),
            'Q' => {
                let mut j = i + 2;
                while let Some(q) = self.at(j) {
                    if q == '\\' && self.at(j + 1) == Some('E') {
                        j += 2;
                        break;
                    }
                    if st.ascii_icase(self.opts) && q.is_ascii_alphabetic() && !in_class {
                        self.push_both_cases(q);
                    } else {
                        self.out.push_str(&regex::escape(&q.to_string()));
                    }
                    j += 1;
                }
                next = j;
            }
            'p' | 'P' => next = self.property(i, n == 'P', uni),
            '1'..='9' => {
                if st.icase && self.opts.ascii_icase_expansion && !st.unicode_case {
                    self.needs_fallback = true;
                }
                self.out.push('\\');
                self.out.push(n);
            }
            'k' if self.at(i + 2) == Some('<') => {
                // \k<name>: the name is copied verbatim
                self.out.push_str("\\k");
                let mut j = i + 2;
                while let Some(c) = self.at(j) {
                    self.out.push(c);
                    j += 1;
                    if c == '>' {
                        break;
                    }
                }
                next = j;
            }
            'x' | 'u' | 'c' | '0' | 'N' => {
                self.out.push('\\');
                self.out.push(n);
                // the argument (hex digits, a braced code point, a control letter) is copied verbatim
                let mut j = i + 2;
                if self.at(j) == Some('{') {
                    while let Some(c) = self.at(j) {
                        self.out.push(c);
                        j += 1;
                        if c == '}' {
                            break;
                        }
                    }
                } else {
                    let take = match n {
                        'x' => 2,
                        'u' => 4,
                        'c' => 1,
                        _ => 3,
                    };
                    for _ in 0..take {
                        match self.at(j) {
                            Some(c)
                                if c.is_ascii_hexdigit()
                                    || (n == 'c' && c.is_ascii_alphabetic()) =>
                            {
                                self.out.push(c);
                                j += 1;
                            }
                            _ => break,
                        }
                    }
                }
                next = j;
            }
            other => {
                self.out.push('\\');
                self.out.push(other);
            }
        }
        next
    }

    /// `\p{..}` / `\P{..}` / `\pL`. Java POSIX names are translated, every other property is copied verbatim.
    fn property(&mut self, i: usize, negated: bool, uni: bool) -> usize {
        let letter = if negated { 'P' } else { 'p' };
        if self.at(i + 2) != Some('{') {
            self.out.push('\\');
            self.out.push(letter);
            if let Some(c) = self.at(i + 2) {
                self.out.push(c);
                return i + 3;
            }
            return i + 2;
        }
        let mut j = i + 3;
        let mut name = String::new();
        while let Some(c) = self.at(j) {
            j += 1;
            if c == '}' {
                break;
            }
            name.push(c);
        }
        match POSIX.iter().find(|(java, _, _)| *java == name) {
            Some((_, ascii, unicode)) => {
                let positive = if uni { *unicode } else { *ascii };
                if !negated {
                    self.out.push_str(positive);
                } else if let Some(prop) = positive.strip_prefix(r"\p") {
                    self.out.push_str(r"\P");
                    self.out.push_str(prop);
                } else {
                    self.out.push_str("[^");
                    self.out.push_str(positive);
                    self.out.push(']');
                }
            }
            None => {
                self.out.push('\\');
                self.out.push(letter);
                self.out.push('{');
                self.out.push_str(&name);
                self.out.push('}');
            }
        }
        j
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(p: &str) -> String {
        translate(
            p,
            Options {
                java_dollar: false,
                ascii_icase_expansion: true,
            },
        )
        .pattern
    }

    #[test]
    fn rust_u_flag_is_never_emitted() {
        for p in ["(?U)a+", "(?iU)a", "x(?U)y", "(?U:a)", "(?-U)a", "(?i-U)a"] {
            let out = t(p);
            assert!(!out.contains('U') || out.contains(r"\U"), "{p} -> {out}");
            assert!(
                !out.contains("(?U") && !out.contains("(?iU"),
                "{p} -> {out}"
            );
        }
    }

    #[test]
    fn unicode_character_class_scope() {
        assert_eq!(t(r"(?U)\w+"), r"\w+");
        assert_eq!(t(r"\w+"), "[a-zA-Z0-9_]+");
        assert_eq!(t(r"(?U)\w(?-U)\w"), r"\w[a-zA-Z0-9_]");
        assert_eq!(t(r"((?U)\w)\w"), r"(\w)[a-zA-Z0-9_]");
        assert_eq!(t(r"(?U:\w)\w"), r"(?:\w)[a-zA-Z0-9_]");
        assert_eq!(t(r"(?U)[\s\x1c-\x1f]+"), r"[\s\x1c-\x1f]+");
        assert_eq!(t(r"(?U)\b"), r"\b");
        assert_eq!(t(r"(?U)\p{Alpha}"), r"\p{Alphabetic}");
    }

    #[test]
    fn ascii_ignore_case_is_expanded() {
        assert_eq!(t("(?i)ab"), "[aA][bB]");
        assert_eq!(t("(?i)РАЙОН"), "РАЙОН");
        assert_eq!(t("(?i)[a-c]"), "[a-cA-C]");
        assert_eq!(t("(?i)[^x]"), "[^xX]");
        assert_eq!(t("(?i:a)b"), "(?:[aA])b");
        assert_eq!(t("(?i)a(?-i)b"), "[aA]b");
        assert_eq!(t("(?i)(?<w>a)"), "(?<w>[aA])");
        // Unicode folding when u or U is set: the Rust flag does the work
        assert_eq!(t("(?iu)ab"), "(?i)ab");
        assert_eq!(t("(?iU)ab"), "(?i)ab");
        assert_eq!(t("(?i)a(?u)b"), "[aA](?i)b");
    }

    #[test]
    fn dollar_variant_is_reported_and_not_applied_in_multiline() {
        let d = |p: &str, java| {
            translate(
                p,
                Options {
                    java_dollar: java,
                    ascii_icase_expansion: true,
                },
            )
        };
        assert!(d("a$", false).has_dollar);
        assert_eq!(d("a$", false).pattern, "a$");
        assert!(d("a$", true).pattern.contains(r"(?=\r\n\z)"));
        assert!(!d("(?m)a$", false).has_dollar);
        assert!(!d(r"a\$", false).has_dollar);
        assert!(!d("[$]", false).has_dollar);
    }

    #[test]
    fn backreference_under_ascii_ignore_case_asks_for_the_flag() {
        let o = |expand| Options {
            java_dollar: false,
            ascii_icase_expansion: expand,
        };
        assert!(translate(r"(?i)(a)\1", o(true)).needs_icase_fallback);
        assert_eq!(translate(r"(?i)(a)\1", o(false)).pattern, r"(?i)(a)\1");
        assert!(!translate(r"(a)\1", o(true)).needs_icase_fallback);
    }
}
