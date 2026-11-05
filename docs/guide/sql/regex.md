---
title: Regular Expressions
rank: 6
---

# Regular Expressions

Spark evaluates `RLIKE`/`regexp_like`, `regexp_replace`, `regexp_extract`, `regexp_extract_all` and `split` with `java.util.regex`.
Sail uses Rust regex engines for the same functions and follows the Java behavior as closely as it can.

## Engines

A pattern is compiled with the [`regex`](https://docs.rs/regex) crate first. It is fast and linear-time, but it supports neither
look-around (`(?=…)`, `(?!…)`, `(?<=…)`, `(?<!…)`) nor back-references (`\1`).
When `regex` rejects a pattern, Sail compiles it with [`fancy-regex`](https://docs.rs/fancy-regex), a backtracking engine with the same
syntax family that supports both. A pattern that neither engine accepts fails with an `invalid regex pattern` error.

Patterns that `regex` accepts never touch the backtracking engine, so existing queries keep their speed. A pattern given as a column is
compiled once per distinct value in a batch.
The backtracking engine is limited to 500 million steps per match, which only a pathological pattern reaches.
Java has no such limit and would run until the JVM gives up, so a pattern that Spark finishes after a very long time fails here with an error.

## What matches Java

- `\d`, `\D`, `\w`, `\W`, `\s`, `\S`, `\b` and `\B` are ASCII-only, as in Java without `UNICODE_CHARACTER_CLASS` (`\b` since JDK 19).
  `'привет' RLIKE '^\w+$'` is `false`, not `true` as in the Rust engines' own default.
- Flag groups are parsed, not passed through. `(?U)` is Java's `UNICODE_CHARACTER_CLASS`: inside its scope `\w \d \s \b` and the POSIX classes are Unicode
  (`'привет' RLIKE '(?U)^\w+$'` is `true`). It does not swap greediness, which is what `U` means in the Rust engines and is never passed on.
  Leading, inline (`x(?U)y`), scoped (`(?U:..)`), combined (`(?iU)`) and negated (`(?-U)`) groups are supported.
- `(?i)` folds ASCII letters only, as in Java, unless `u` or `U` is also set (`'район' RLIKE '(?i)РАЙОН'` is `false`, `'район' RLIKE '(?iu)РАЙОН'` is `true`).
- `$` also matches before a final line terminator, as on JDK 21: `\n`, `\r\n`, `\r`, `\u0085`, `\u2028` and `\u2029` (not `\u000b`/`\u000c`, not before two terminators). Only texts that end in one pay for it. Under `(?d)` only `\n` counts.
- `.` excludes every Java line terminator (`\n`, `\r`, `\u0085`, `\u2028`, `\u2029`; only `\n` under `(?d)`) unless `(?s)` is set.
- `\p{javaUpperCase}`-style classes, `\p{InCyrillic}` blocks (every `Character.UnicodeBlock` of JDK 21, names read like `UnicodeBlock.forName`), `\p{IsAlphabetic}`, `\p{IsCyrillic}` and `\p{IsL}`, and the `script=`, `block=`, `gc=` forms are translated. A block name Java does not know is an error, as in Java.
- Patterns Java rejects are rejected: an unclosed class (`[a`), a `{` that does not start a quantifier (`a{2`, `a{,2}`). A back-reference to a group that does not exist (`(a)\2`) compiles and never matches.
- Empty matches follow `Matcher.find()`. For example `regexp_replace('abc', 'b*', '-')` is `-a--c-`, and `regexp_extract_all('abc', 'b*', 0)` is `['', 'b', '', '']`.
- `regexp_replace` replacement strings use the Java syntax: `$n` is a group reference (digits are consumed while the group exists, so `$10` is group 1 followed by `0` when there is no group 10), `${name}` refers to a named group, and `\x` is a literal `x`, so `\$` is a dollar sign. A group that did not participate in the match is replaced by the empty string.
- `split` follows `Pattern.split`: a zero-width match at the start of the string does not produce a leading empty string, a positive `limit` caps the number of parts, and a `limit` of `0` or less keeps trailing empty strings, as Spark does. An empty pattern splits the string into characters (with a `limit`, the first `limit` characters).
- `regexp_extract` and `regexp_extract_all` return an empty string for a group that did not participate, and `NULL` for `NULL` input.
- A back-reference to a group that did not participate fails the match, as in Java.

## Known differences

These behaviors of `java.util.regex` are not reproduced. Each is checked against Spark in the test suite, so the list is measured, not assumed.

- Unicode data: the JDK 21 uses Unicode 15.0 and the Rust engines Unicode 16. Characters assigned after 15.0 are outside what Java matches, so `\p{L}` and friends match them here. For code points both assign, every Java class in the table checked against the JDK matches exactly except `Alphabetic` (33 more here, which also affects `\p{javaAlphabetic}`, `(?U)\p{Alpha}`, `(?U)\p{Alnum}`), `Bidi_Mirrored` (1 more), and the identifier classes `\p{javaUnicodeIdentifierStart}` (6 fewer) and `\p{javaUnicodeIdentifierPart}` (16 fewer), which are approximated from general categories.
- In `(?m)` mode `^` and `$` see only `\n` as a line terminator, not `\r`, `\u0085`, `\u2028`, `\u2029`. `(?c)` is ignored.
- Case-insensitive back-references under `(?iu)`/`(?iU)` use the Rust engine's Unicode folding, which differs from Java for the dotted/dotless i (`ı`, `İ`).
- A `{` at the start of a pattern or after `(` or `|` (`{1}`) is passed to the engine as it is; Java accepts some of these.
- Inside a character class combined with `&&`, `(?i)` adds the other case of a letter to the whole class instead of to the left operand only.
- Possessive quantifiers (`a++`, `a*+`, `a?+`) are accepted but are not possessive: `'aaa' RLIKE 'a++a'` is `false` in Spark and `true` here. Atomic groups (`(?>…)`) work as in Java.
- `regexp_count`, `regexp_instr` and `regexp_substr` still use the `regex` crate only, so look-around and back-references are not available in them. The talos-pipelines transforms do not use them.
