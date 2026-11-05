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
- `\p{Alpha}`, `\p{Digit}` and the other POSIX classes (ASCII-only in Java), `\h`/`\H` and `\Q…\E` quoting are translated to the Rust syntax.
- Empty matches follow `Matcher.find()`. For example `regexp_replace('abc', 'b*', '-')` is `-a--c-`, and `regexp_extract_all('abc', 'b*', 0)` is `['', 'b', '', '']`.
- `regexp_replace` replacement strings use the Java syntax: `$n` is a group reference (digits are consumed while the group exists, so `$10` is group 1 followed by `0` when there is no group 10), `${name}` refers to a named group, and `\x` is a literal `x`, so `\$` is a dollar sign. A group that did not participate in the match is replaced by the empty string.
- `split` follows `Pattern.split`: a zero-width match at the start of the string does not produce a leading empty string, a positive `limit` caps the number of parts, and a `limit` of `0` or less keeps trailing empty strings, as Spark does. An empty pattern splits the string into characters (with a `limit`, the first `limit` characters).
- `regexp_extract` and `regexp_extract_all` return an empty string for a group that did not participate, and `NULL` for `NULL` input.
- A back-reference to a group that did not participate fails the match, as in Java.

## Known differences

These behaviors of `java.util.regex` are not reproduced. Each is checked against Spark in the test suite, so the list is measured, not assumed.

- `$` and `.` keep Rust semantics. Java's `$` also matches before a final line terminator (`'abc\n' RLIKE 'c$'` is `true` in Spark and `false` here), and Java's `.` excludes `\r`, `\u0085`, `\u2028` and `\u2029` as well as `\n`.
- `(?i)` case folding is Unicode-aware, while Java folds only ASCII unless `(?iu)` is given (`'ПРИВЕТ' RLIKE '(?i)привет'` is `false` in Spark and `true` here).
- Possessive quantifiers (`a++`, `a*+`, `a?+`) are accepted but are not possessive: `'aaa' RLIKE 'a++a'` is `false` in Spark and `true` here. Atomic groups (`(?>…)`) work as in Java.
- `regexp_count`, `regexp_instr` and `regexp_substr` still use the `regex` crate only, so look-around and back-references are not available in them. The talos-pipelines transforms do not use them.
