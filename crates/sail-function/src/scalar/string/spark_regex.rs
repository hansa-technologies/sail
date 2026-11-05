//! Regex engine shared by the Spark regex functions (`rlike`, `regexp_replace`,
//! `regexp_extract`, `regexp_extract_all`, `split`).
//!
//! Spark evaluates these with `java.util.regex`, which supports look-around and
//! back-references. The `regex` crate supports neither, but it is much faster,
//! so a pattern is compiled with `regex` first and only falls back to
//! `fancy-regex` (same syntax family, backtracking, supports both) when `regex`
//! rejects it. A pattern that neither engine accepts is reported as invalid.
//!
//! Match iteration follows `java.util.regex.Matcher.find()` rather than the
//! `regex` crate's iterator, so that empty matches land where Java puts them
//! (for example `"abc".replaceAll("b*", "-")` is `-a--c-` in Java and `-a-c-`
//! with `Regex::replace_all`).
//!
//! Known differences from Java that are not translated here (they hold for the
//! fast path as well) are documented in `docs/` under "Regex compatibility".

use datafusion_common::{DataFusionError, Result};

/// Upper bound on backtracking steps for the `fancy-regex` fallback. The crate's
/// default (1M) is far too low for realistic Foundry patterns over long text;
/// Java has no limit at all, so this only guards against catastrophic patterns.
const FANCY_BACKTRACK_LIMIT: usize = 500_000_000;

/// Capture group spans of one match; index 0 is the whole match.
pub type Spans = Vec<Option<(usize, usize)>>;

#[derive(Debug, Clone)]
enum Engine {
    Fast(regex::Regex),
    Fancy(Box<fancy_regex::Regex>),
}

#[derive(Debug, Clone)]
pub struct SparkRegex {
    engine: Engine,
    pattern: String,
}

impl SparkRegex {
    /// Compiles `pattern` with `regex`, falling back to `fancy-regex`.
    pub fn new(function_name: &str, pattern: &str) -> Result<Self> {
        let translated = java_to_rust_syntax(pattern);
        let engine = match regex::Regex::new(&translated) {
            Ok(re) => Engine::Fast(re),
            Err(fast_err) => fancy_regex::RegexBuilder::new(&translated)
                .backtrack_limit(FANCY_BACKTRACK_LIMIT)
                .build()
                .map(|re| Engine::Fancy(Box::new(re)))
                .map_err(|_| {
                    DataFusionError::Execution(format!(
                        "{function_name}: invalid regex pattern `{pattern}`: {fast_err}"
                    ))
                })?,
        };
        Ok(Self {
            engine,
            pattern: pattern.to_string(),
        })
    }

    /// The pattern as written by the user.
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// Capture group names by group index (index 0 is the whole match, always unnamed).
    pub fn group_names(&self) -> Vec<Option<String>> {
        match &self.engine {
            Engine::Fast(re) => re.capture_names().map(|n| n.map(str::to_string)).collect(),
            Engine::Fancy(re) => re.capture_names().map(|n| n.map(str::to_string)).collect(),
        }
    }

    /// Whether the fallback engine is in use (exposed for tests and metrics).
    pub fn is_fancy(&self) -> bool {
        matches!(self.engine, Engine::Fancy(_))
    }

    /// Number of capture groups, excluding the whole match.
    pub fn group_count(&self) -> usize {
        match &self.engine {
            Engine::Fast(re) => re.captures_len() - 1,
            Engine::Fancy(re) => re.captures_len() - 1,
        }
    }

    /// Java `Matcher.find()`: a match anywhere in `text`.
    pub fn is_match(&self, text: &str) -> Result<bool> {
        match &self.engine {
            Engine::Fast(re) => Ok(re.is_match(text)),
            Engine::Fancy(re) => re.is_match(text).map_err(runtime_err),
        }
    }

    /// First match starting the search at byte offset `start`. The whole text is
    /// the context, so anchors and look-behind see what precedes `start`.
    pub fn captures_at(&self, text: &str, start: usize) -> Result<Option<Spans>> {
        match &self.engine {
            Engine::Fast(re) => Ok(re.captures_at(text, start).map(|caps| {
                (0..caps.len())
                    .map(|i| caps.get(i).map(|m| (m.start(), m.end())))
                    .collect()
            })),
            Engine::Fancy(re) => {
                Ok(re
                    .captures_from_pos(text, start)
                    .map_err(runtime_err)?
                    .map(|caps| {
                        (0..caps.len())
                            .map(|i| caps.get(i).map(|m| (m.start(), m.end())))
                            .collect()
                    }))
            }
        }
    }

    /// All matches with `Matcher.find()` iteration. After an empty match the
    /// search resumes one character later; after a non-empty match it resumes at
    /// the match end, where an empty match is allowed again (unlike `regex`).
    pub fn find_all(&self, text: &str) -> Result<Vec<Spans>> {
        let mut out = Vec::new();
        let mut start = 0;
        while start <= text.len() {
            let Some(spans) = self.captures_at(text, start)? else {
                break;
            };
            let (m_start, m_end) = spans[0].unwrap_or((start, start));
            start = if m_end == m_start {
                match text[m_end..].chars().next() {
                    Some(c) => m_end + c.len_utf8(),
                    None => text.len() + 1,
                }
            } else {
                m_end
            };
            out.push(spans);
        }
        Ok(out)
    }

    /// First match, `Matcher.find()`.
    pub fn find_first(&self, text: &str) -> Result<Option<Spans>> {
        self.captures_at(text, 0)
    }

    /// Spark's `split(str, regex, limit)` (`UTF8String.split`).
    ///
    /// `Pattern.split` semantics: a leading empty string is dropped for a
    /// zero-width match at index 0, and `limit > 0` caps the number of parts with
    /// the remainder in the last one. Spark maps `limit == 0` to `-1`, so trailing
    /// empty strings are always kept (unlike Java's own `limit == 0`).
    ///
    /// An empty pattern on a non-empty string is special-cased by Spark: one
    /// part per character, at most `limit` parts when `0 < limit < length`, and
    /// the last of those parts is a single character, not the remainder.
    pub fn split(&self, text: &str, limit: i32) -> Result<Vec<String>> {
        if self.pattern.is_empty() && !text.is_empty() {
            let chars = text.chars().count();
            let n = if limit <= 0 || limit as usize > chars {
                chars
            } else {
                limit as usize
            };
            return Ok(text.chars().take(n).map(String::from).collect());
        }
        let limited = limit > 0;
        let mut parts: Vec<&str> = Vec::new();
        let mut index = 0;
        for spans in self.find_all(text)? {
            if limited && parts.len() + 1 >= limit as usize {
                break;
            }
            let (m_start, m_end) = spans[0].unwrap_or((0, 0));
            if index == 0 && m_start == 0 && m_end == 0 {
                continue;
            }
            parts.push(&text[index..m_start]);
            index = m_end;
        }
        if index == 0 && parts.is_empty() {
            return Ok(vec![text.to_string()]);
        }
        parts.push(&text[index..]);
        Ok(parts.into_iter().map(str::to_string).collect())
    }

    /// `Matcher.replaceAll(replacement)` with Java replacement syntax.
    pub fn replace_all(&self, text: &str, replacement: &JavaReplacement) -> Result<String> {
        let matches = self.find_all(text)?;
        if matches.is_empty() {
            return Ok(text.to_string());
        }
        let mut out = String::with_capacity(text.len());
        let mut last = 0;
        for spans in matches {
            let (m_start, m_end) = spans[0].unwrap_or((last, last));
            out.push_str(&text[last..m_start]);
            replacement.expand(text, &spans, &mut out)?;
            last = m_end;
        }
        out.push_str(&text[last..]);
        Ok(out)
    }
}

fn runtime_err(err: fancy_regex::Error) -> DataFusionError {
    DataFusionError::Execution(format!("regex evaluation failed: {err}"))
}

/// A parsed `Matcher.appendReplacement` replacement string: `$n` (greedy while
/// the group exists), `${name}` and `\x` (escapes `x`).
#[derive(Debug, Clone)]
pub struct JavaReplacement {
    parts: Vec<ReplacementPart>,
}

#[derive(Debug, Clone)]
enum ReplacementPart {
    Literal(String),
    Group(usize),
}

impl JavaReplacement {
    pub fn parse(function_name: &str, replacement: &str, regex: &SparkRegex) -> Result<Self> {
        let group_count = regex.group_count();
        let mut parts = Vec::new();
        let mut literal = String::new();
        let mut chars = replacement.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\\' => match chars.next() {
                    Some(next) => literal.push(next),
                    None => {
                        return Err(DataFusionError::Execution(format!(
                            "{function_name}: character to be escaped is missing"
                        )));
                    }
                },
                '$' if chars.peek() == Some(&'{') => {
                    chars.next();
                    let name: String = chars.by_ref().take_while(|c| *c != '}').collect();
                    let group = regex
                        .group_names()
                        .iter()
                        .position(|n| n.as_deref() == Some(name.as_str()))
                        .ok_or_else(|| {
                            DataFusionError::Execution(format!(
                                "{function_name}: No group with name {{{name}}}"
                            ))
                        })?;
                    if !literal.is_empty() {
                        parts.push(ReplacementPart::Literal(std::mem::take(&mut literal)));
                    }
                    parts.push(ReplacementPart::Group(group));
                }
                '$' => {
                    let first = chars.next().and_then(|d| d.to_digit(10)).ok_or_else(|| {
                        DataFusionError::Execution(format!(
                            "{function_name}: illegal group reference in replacement `{replacement}`"
                        ))
                    })? as usize;
                    if first > group_count {
                        return Err(DataFusionError::Execution(format!(
                            "{function_name}: No group {first}"
                        )));
                    }
                    let mut group = first;
                    // Java keeps consuming digits while the group number exists.
                    while let Some(d) = chars.peek().and_then(|d| d.to_digit(10)) {
                        let next = group * 10 + d as usize;
                        if next > group_count {
                            break;
                        }
                        group = next;
                        chars.next();
                    }
                    if !literal.is_empty() {
                        parts.push(ReplacementPart::Literal(std::mem::take(&mut literal)));
                    }
                    parts.push(ReplacementPart::Group(group));
                }
                other => literal.push(other),
            }
        }
        if !literal.is_empty() {
            parts.push(ReplacementPart::Literal(literal));
        }
        Ok(Self { parts })
    }

    fn expand(&self, text: &str, spans: &Spans, out: &mut String) -> Result<()> {
        for part in &self.parts {
            match part {
                ReplacementPart::Literal(s) => out.push_str(s),
                ReplacementPart::Group(g) => {
                    if let Some(Some((s, e))) = spans.get(*g) {
                        out.push_str(&text[*s..*e]);
                    }
                }
            }
        }
        Ok(())
    }
}

/// Java POSIX character classes (`\p{Alpha}`), which are ASCII-only in Java, as `regex` ASCII bracket classes.
const POSIX_CLASSES: &[(&str, &str)] = &[
    ("Lower", "lower"),
    ("Upper", "upper"),
    ("ASCII", "ascii"),
    ("Alpha", "alpha"),
    ("Digit", "digit"),
    ("Alnum", "alnum"),
    ("Punct", "punct"),
    ("Graph", "graph"),
    ("Print", "print"),
    ("Blank", "blank"),
    ("Cntrl", "cntrl"),
    ("XDigit", "xdigit"),
    ("Space", "space"),
];

/// Java's `\h`: horizontal whitespace.
const HSPACE: &str = r"\x20\t\x{A0}\x{1680}\x{180E}\x{2000}-\x{200A}\x{202F}\x{205F}\x{3000}";
/// Java's `\s`: `[ \t\n\x0B\f\r]`.
const SPACE: &str = r" \t\n\x0B\f\r";

/// Rewrites Java regex syntax that `regex`/`fancy-regex` read differently, or not at all, into the equivalent they understand:
///
/// - `\d \D \w \W \s \S \b \B` are ASCII-only in Java (without `UNICODE_CHARACTER_CLASS`; `\b` since JDK 19) but Unicode-aware
///   here. `\w` matching Cyrillic letters, for example, would make `'привет' RLIKE '^\w+$'` true here and false in Spark.
/// - `\p{Alpha}`-style POSIX classes (ASCII-only in Java) become `regex`'s ASCII bracket classes, `\h`/`\H` are expanded.
/// - `\Q…\E` quotes literally.
///
/// An escaped backslash is never mistaken for an escape. Everything else, including patterns that are invalid, passes through
/// unchanged, so those fail the same way they always did.
fn java_to_rust_syntax(pattern: &str) -> std::borrow::Cow<'_, str> {
    if !pattern.contains('\\') {
        return std::borrow::Cow::Borrowed(pattern);
    }
    let mut out = String::with_capacity(pattern.len() + 16);
    let mut chars = pattern.chars().peekable();
    let mut class_depth = 0usize;
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                let Some(next) = chars.next() else {
                    out.push('\\');
                    break;
                };
                let in_class = class_depth > 0;
                match next {
                    'd' => out.push_str(if in_class { "0-9" } else { "[0-9]" }),
                    'D' => out.push_str("[^0-9]"),
                    'w' => out.push_str(if in_class {
                        "a-zA-Z0-9_"
                    } else {
                        "[a-zA-Z0-9_]"
                    }),
                    'W' => out.push_str("[^a-zA-Z0-9_]"),
                    's' if in_class => out.push_str(SPACE),
                    's' => out.push_str(&format!("[{SPACE}]")),
                    'S' => out.push_str(&format!("[^{SPACE}]")),
                    'h' if in_class => out.push_str(HSPACE),
                    'h' => out.push_str(&format!("[{HSPACE}]")),
                    'H' => out.push_str(&format!("[^{HSPACE}]")),
                    'b' if !in_class => out.push_str(r"(?-u:\b)"),
                    'B' if !in_class => out.push_str(r"(?-u:\B)"),
                    'Q' => {
                        let mut quoted = String::new();
                        while let Some(q) = chars.next() {
                            if q == '\\' && chars.peek() == Some(&'E') {
                                chars.next();
                                break;
                            }
                            quoted.push(q);
                        }
                        out.push_str(&regex::escape(&quoted));
                    }
                    'p' | 'P' => {
                        let rest: String = chars.clone().collect();
                        let class = rest
                            .strip_prefix('{')
                            .and_then(|r| r.split_once('}'))
                            .and_then(|(name, _)| {
                                POSIX_CLASSES.iter().find(|(java, _)| *java == name)
                            });
                        match class {
                            Some((java, ascii)) => {
                                for _ in 0..java.chars().count() + 2 {
                                    chars.next();
                                }
                                let negate = next == 'P';
                                match (in_class, negate) {
                                    (true, false) => out.push_str(&format!("[:{ascii}:]")),
                                    (true, true) => out.push_str(&format!("[^[:{ascii}:]]")),
                                    (false, false) => out.push_str(&format!("[[:{ascii}:]]")),
                                    (false, true) => out.push_str(&format!("[^[:{ascii}:]]")),
                                }
                            }
                            None => {
                                out.push('\\');
                                out.push(next);
                            }
                        }
                    }
                    _ => {
                        out.push('\\');
                        out.push(next);
                    }
                }
            }
            '[' => {
                class_depth += 1;
                out.push(c);
            }
            ']' if class_depth > 0 => {
                class_depth -= 1;
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    std::borrow::Cow::Owned(out)
}

#[cfg(test)]
#[expect(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn re(p: &str) -> SparkRegex {
        SparkRegex::new("test", p).unwrap()
    }

    #[test]
    fn falls_back_for_lookaround_and_backreference_only() {
        assert!(!re(r"a+b").is_fancy());
        assert!(re(r"foo(?=bar)").is_fancy());
        assert!(re(r"(?<=\d)x").is_fancy());
        assert!(re(r"(\w)\1").is_fancy());
        assert!(SparkRegex::new("test", "(").is_err());
    }

    #[test]
    fn lookaround_and_backreference_match() {
        assert!(re(r"foo(?=bar)").is_match("foobar").unwrap());
        assert!(!re(r"foo(?=bar)").is_match("foobaz").unwrap());
        assert!(re(r"(\w)\1").is_match("hello").unwrap());
        assert!(!re(r"(\w)\1").is_match("helo").unwrap());
    }

    #[test]
    fn find_all_follows_java_empty_match_rules() {
        let r = re("b*");
        let r = JavaReplacement::parse("t", "-", &r)
            .map(|rep| (r, rep))
            .unwrap();
        assert_eq!(r.0.replace_all("abc", &r.1).unwrap(), "-a--c-");
    }

    #[test]
    fn split_matches_java() {
        let split = |p: &str, s: &str, l: i32| re(p).split(s, l).unwrap();
        assert_eq!(split(",", "a,b,,", -1), ["a", "b", "", ""]);
        assert_eq!(split(",", "a,b,,", 0), ["a", "b", "", ""]);
        assert_eq!(split(",", "a,b,c", 2), ["a", "b,c"]);
        assert_eq!(split("", "abc", -1), ["a", "b", "c"]);
        assert_eq!(split("", "abc", 2), ["a", "b"]);
        assert_eq!(split("", "", -1), [""]);
        assert_eq!(split("(?=a)", "abc", -1), ["abc"]);
        assert_eq!(split(",", ",a", -1), ["", "a"]);
        assert_eq!(split("(?<=,)", "a,b", -1), ["a,", "b"]);
    }

    #[test]
    fn class_escapes_follow_java_ascii_defaults() {
        assert!(!re(r"^\w+$").is_match("привет").unwrap());
        assert!(re(r"^\w+$").is_match("abc_1").unwrap());
        assert!(!re(r"\d").is_match("٣").unwrap());
        assert!(re(r"^[\w.-]+$").is_match("a.b-c").unwrap());
        assert!(!re(r"^[\w.-]+$").is_match("я").unwrap());
        assert!(re(r"[^\d]").is_match("a").unwrap());
        assert!(!re(r"[^\d]").is_match("12").unwrap());
        assert!(re(r"\D").is_match("a").unwrap());
        assert!(re(r"\S+").is_match("x").unwrap());
        assert!(re(r"a\sb").is_match("a\x0Bb").unwrap());
        assert!(!re(r"\bабв\b").is_match("абв").unwrap());
        // An escaped backslash is not a class escape.
        assert!(re(r"\\d").is_match("\\d").unwrap());
        // The translation must survive the look-around fallback as well.
        assert!(!re(r"(?<=a)\w").is_match("aя").unwrap());
    }

    #[test]
    fn java_only_syntax_is_translated() {
        assert!(re(r"\Qa.b\E").is_match("a.b").unwrap());
        assert!(!re(r"\Qa.b\E").is_match("axb").unwrap());
        assert!(re(r"^\p{Alpha}+$").is_match("abc").unwrap());
        assert!(!re(r"^\p{Alpha}+$").is_match("абв").unwrap());
        assert!(re(r"^[\p{Alpha}\d]+$").is_match("a1").unwrap());
        assert!(re(r"\P{Digit}").is_match("a").unwrap());
        assert!(!re(r"^\P{Digit}+$").is_match("a1").unwrap());
        assert!(re(r"a\hb").is_match("a b").unwrap());
        assert!(re(r"a\hb").is_match("a\tb").unwrap());
        assert!(!re(r"a\hb").is_match("a\nb").unwrap());
    }

    #[test]
    fn replacement_syntax() {
        let r = re(r"(\d)(\d)");
        let rep = JavaReplacement::parse("t", r"$2$1\$", &r).unwrap();
        assert_eq!(r.replace_all("12 34", &rep).unwrap(), "21$ 43$");
        assert!(JavaReplacement::parse("t", "$3", &r).is_err());
        let named = re(r"(?<x>a)(b)");
        let rep = JavaReplacement::parse("t", "${x}-$2", &named).unwrap();
        assert_eq!(named.replace_all("ab", &rep).unwrap(), "a-b");
        assert!(JavaReplacement::parse("t", "${nope}", &named).is_err());
    }
}
