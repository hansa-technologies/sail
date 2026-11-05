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

use std::sync::{Arc, OnceLock};

use datafusion_common::{DataFusionError, Result};

use super::java_syntax::{Options, translate};

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

impl Engine {
    /// `regex` first, `fancy-regex` when it rejects the pattern. The error of the last engine tried is returned, because it is the one
    /// that says what is wrong (`regex` alone would call a valid back-reference unsupported).
    fn compile(pattern: &str) -> std::result::Result<Self, String> {
        match regex::Regex::new(pattern) {
            Ok(re) => Ok(Self::Fast(re)),
            Err(_) => fancy_regex::RegexBuilder::new(pattern)
                .backtrack_limit(FANCY_BACKTRACK_LIMIT)
                .build()
                .map(|re| Self::Fancy(Box::new(re)))
                .map_err(|e| e.to_string()),
        }
    }
}

/// Compiled lazily, once per regex, the first time a text ending in a line terminator is searched.
type LazyEngine = Arc<OnceLock<Option<Engine>>>;

#[derive(Debug, Clone)]
pub struct SparkRegex {
    engine: Engine,
    pattern: String,
    /// Java's `$` also matches before a final line terminator. That needs a look-around, which would put every pattern with a `$` on
    /// the slow engine, so the plain engine is used unless the text ends in a line terminator, where this variant takes over.
    java_dollar: Option<(String, LazyEngine)>,
}

impl SparkRegex {
    /// Compiles `pattern` (Java syntax) with `regex`, falling back to `fancy-regex`.
    pub fn new(function_name: &str, pattern: &str) -> Result<Self> {
        let mut opts = Options {
            java_dollar: false,
            ascii_icase_expansion: true,
        };
        let mut t = translate(pattern, opts);
        if t.needs_icase_fallback {
            opts.ascii_icase_expansion = false;
            t = translate(pattern, opts);
        }
        let engine = Engine::compile(&t.pattern).map_err(|e| {
            DataFusionError::Execution(format!(
                "{function_name}: invalid regex pattern `{pattern}`: {e}"
            ))
        })?;
        let java_dollar = t.has_dollar.then(|| {
            opts.java_dollar = true;
            (translate(pattern, opts).pattern, Arc::new(OnceLock::new()))
        });
        Ok(Self {
            engine,
            pattern: pattern.to_string(),
            java_dollar,
        })
    }

    /// The engine for `text`: the Java-`$` variant when the text ends in a line terminator and the pattern has a `$`.
    fn engine_for(&self, text: &str) -> &Engine {
        match &self.java_dollar {
            Some((variant, cell))
                if text.ends_with(['\n', '\r', '\u{85}', '\u{2028}', '\u{2029}']) =>
            {
                cell.get_or_init(|| Engine::compile(variant).ok())
                    .as_ref()
                    .unwrap_or(&self.engine)
            }
            _ => &self.engine,
        }
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
        match self.engine_for(text) {
            Engine::Fast(re) => Ok(re.is_match(text)),
            Engine::Fancy(re) => re.is_match(text).map_err(runtime_err),
        }
    }

    /// First match starting the search at byte offset `start`. The whole text is
    /// the context, so anchors and look-behind see what precedes `start`.
    pub fn captures_at(&self, text: &str, start: usize) -> Result<Option<Spans>> {
        match self.engine_for(text) {
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

    /// Span of the first match at or after byte offset `start`, without capture groups (cheaper than `captures_at`).
    fn find_range_at(&self, text: &str, start: usize) -> Result<Option<(usize, usize)>> {
        match self.engine_for(text) {
            Engine::Fast(re) => Ok(re.find_at(text, start).map(|m| (m.start(), m.end()))),
            Engine::Fancy(re) => Ok(re
                .find_from_pos(text, start)
                .map_err(runtime_err)?
                .map(|m| (m.start(), m.end()))),
        }
    }

    /// Spans of all matches with `Matcher.find()` iteration, without capture groups; see [`Self::find_all`].
    pub fn find_all_ranges(&self, text: &str) -> Result<Vec<(usize, usize)>> {
        let mut out = Vec::new();
        let mut start = 0;
        while start <= text.len() {
            let Some((m_start, m_end)) = self.find_range_at(text, start)? else {
                break;
            };
            start = next_search_start(text, m_start, m_end);
            out.push((m_start, m_end));
        }
        Ok(out)
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
            start = next_search_start(text, m_start, m_end);
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
        for (m_start, m_end) in self.find_all_ranges(text)? {
            if limited && parts.len() + 1 >= limit as usize {
                break;
            }
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
        let mut out = String::with_capacity(text.len());
        let mut last = 0;
        if replacement.uses_groups() {
            for spans in self.find_all(text)? {
                let (m_start, m_end) = spans[0].unwrap_or((last, last));
                out.push_str(&text[last..m_start]);
                replacement.expand(text, &spans, &mut out);
                last = m_end;
            }
        } else {
            for (m_start, m_end) in self.find_all_ranges(text)? {
                out.push_str(&text[last..m_start]);
                replacement.expand(text, &Vec::new(), &mut out);
                last = m_end;
            }
        }
        out.push_str(&text[last..]);
        Ok(out)
    }
}

/// Where `Matcher.find()` resumes: at the match end, or one character further after an empty match.
fn next_search_start(text: &str, m_start: usize, m_end: usize) -> usize {
    if m_end != m_start {
        return m_end;
    }
    match text[m_end..].chars().next() {
        Some(c) => m_end + c.len_utf8(),
        None => text.len() + 1,
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

    /// Whether any part refers to a capture group (otherwise matching can skip the capture machinery).
    fn uses_groups(&self) -> bool {
        self.parts
            .iter()
            .any(|p| matches!(p, ReplacementPart::Group(_)))
    }

    fn expand(&self, text: &str, spans: &Spans, out: &mut String) {
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
    }
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
    fn unicode_character_class_flag_is_java_not_rust_swap_greed() {
        // (?U) in Rust would swap greediness; in Java it makes \w \d \s \b Unicode
        let m = |p: &str, t: &str| re(p).find_first(t).unwrap().map(|s| s[0].unwrap());
        assert_eq!(m("(?U)a+", "aaa"), Some((0, 3)));
        assert_eq!(m("(?U)a+?", "aaa"), Some((0, 1)));
        assert!(re(r"(?U)^\w+$").is_match("привет").unwrap());
        assert!(!re(r"^\w+$").is_match("привет").unwrap());
        assert!(re(r"(?iU)^\w+$").is_match("ПРИВЕТ").unwrap());
        assert!(re(r"x(?U)\w").is_match("xя").unwrap());
        assert!(!re(r"(?U)x(?-U)\w").is_match("xя").unwrap());
        assert_eq!(
            re(r"(?U)\s+").split("Иванов  Пётр", -1).unwrap(),
            ["Иванов", "Пётр"]
        );
        assert_eq!(
            re(r"(?U)(?<=\w)(?=\W)|(?<=\W)(?=\w)")
                .split("Иван Петров", -1)
                .unwrap(),
            ["Иван", " ", "Петров"]
        );
        // backreference together with (?U): needs the fallback engine and the Unicode \b
        let addr = re(r"(?U)\b((?:ул|д)\.?)\s*\1\.?\s*");
        assert!(addr.is_fancy());
        assert!(addr.is_match("ул. ул. Ленина").unwrap());
        assert!(!addr.is_match("ул. д. Ленина").unwrap());
    }

    #[test]
    fn ignore_case_is_ascii_only_without_u() {
        assert!(!re(r"(?i)РАЙОН").is_match("район").unwrap());
        assert!(re(r"(?iu)РАЙОН").is_match("район").unwrap());
        assert!(re(r"(?iU)РАЙОН").is_match("район").unwrap());
        assert!(re(r"(?i)abc").is_match("ABC").unwrap());
        assert!(re(r"(?i)[a-c]+").is_match("ABC").unwrap());
        assert!(!re(r"(?i)(?<!ЖИЛ)РАЙОН\.").is_match("район.").unwrap());
        assert!(re(r"(?i)(?<!ЖИЛ)РАЙОН\.").is_match("РАЙОН.").unwrap());
        assert!(re(r"(?i)(a)\1").is_match("aA").unwrap());
    }

    #[test]
    fn dollar_matches_before_a_final_line_terminator_as_in_java() {
        let m = |p: &str, t: &str| re(p).is_match(t).unwrap();
        assert!(m("c$", "abc\n"));
        assert!(m("c$", "abc\r\n"));
        assert!(m("c$", "abc\r"));
        assert!(m("c$", "abc\u{2028}"));
        assert!(!m("c$", "abc\n\n"));
        assert!(m("c$", "abc"));
        assert!(!m("c$", "abcd"));
        // no match between the \r and the \n of a final \r\n
        assert!(!m(r"c\r$", "abc\r\n"));
        assert!(!m("(?m)c$x", "abc"));
        let r = re(r"(?<=\p{L})[\s-]+(\p{L}[\p{L}-]*)$");
        assert_eq!(
            r.find_first("Иван Петров\n").unwrap().unwrap()[1],
            Some((9, 21))
        );
        let rep = JavaReplacement::parse("t", "", &re("c$")).unwrap();
        assert_eq!(re("c$").replace_all("abc\n", &rep).unwrap(), "ab\n");
    }

    #[test]
    fn both_engines_rejecting_reports_the_fancy_error() {
        let err = SparkRegex::new("test", r"(a)\2").unwrap_err().to_string();
        assert!(!err.contains("backreferences are not supported"), "{err}");
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
