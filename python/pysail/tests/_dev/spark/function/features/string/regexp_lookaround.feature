Feature: regex look-around and back-references in rlike, regexp_extract, regexp_extract_all, regexp_replace and split

  These patterns are rejected by the `regex` crate. Sail compiles a pattern with `regex` first and falls back to
  `fancy-regex` when `regex` rejects it, so Java-style look-around and back-references work. Expected values are
  what Apache Spark 3.5.5 (JDK 21) returns.

  Rule: rlike

    Scenario Outline: rlike: <case>
      When query
        """
        SELECT <value> RLIKE <pattern> AS result
        """
      Then query result
        | result   |
        | <result> |

      Examples:
        | case                         | value         | pattern                      | result |
        | positive look-ahead matches  | 'ab12'        | r'[a-z]+(?=[0-9])'           | true   |
        | positive look-ahead fails    | 'ab'          | r'[a-z]+(?=[0-9])'           | false  |
        | leading zeros before a digit | '007'         | '^0+(?=\\d)'                 | true   |
        | only zeros                   | '000'         | '^0+(?=\\d)'                 | true   |
        | no leading zeros             | '7'           | '^0+(?=\\d)'                 | false  |
        | negative look-ahead          | 'ПРОФСОЮЗ'    | r'ПРОФСОЮЗ(?!НЫЙ)'           | true   |
        | negative look-ahead fails    | 'ПРОФСОЮЗНЫЙ' | r'ПРОФСОЮЗ(?!НЫЙ)'           | false  |
        | negative look-behind         | 'X5'          | '(?<![A-Z])\\d'              | false  |
        | back-reference repeats       | '1111'        | '^(\\d)\\1{3}$'               | true   |
        | back-reference differs       | '1112'        | '^(\\d)\\1{3}$'               | false  |
        | NULL input                   | CAST(NULL AS STRING) | r'a(?=b)'             | NULL   |

  Rule: regexp_extract

    Scenario Outline: regexp_extract: <case>
      When query
        """
        SELECT regexp_extract(<args>) AS result
        """
      Then query result
        | result   |
        | <result> |

      Examples:
        | case                              | args                                      | result |
        | look-behind                       | 'price: 42 USD', r'(?<=: )[0-9]+', 0      | 42     |
        | back-reference whole match        | 'xabab', '(ab)\\1', 0                     | abab   |
        | back-reference group              | 'xabab', '(ab)\\1', 1                     | ab     |
        | look-ahead with a capture group   | 'item-42-end', '(\\d+)(?=-end)', 1        | 42     |
        | look-ahead without a match        | 'item-42-x', '(\\d+)(?=-end)', 1          |        |
        | non-participating group is empty  | 'b', r'(a)?(b)(?=$)', 1                   |        |

  Rule: regexp_extract_all

    Scenario Outline: regexp_extract_all: <case>
      When query
        """
        SELECT regexp_extract_all(<args>) AS result
        """
      Then query result
        | result   |
        | <result> |

      Examples:
        | case                         | args                              | result       |
        | look-ahead                   | 'a1 b2 c', '[a-z](?=\\d)', 0      | [a, b]       |
        | back-reference whole match   | 'aa bb cd ee', '(\\w)\\1', 0       | [aa, bb, ee] |
        | back-reference group         | 'aa bb cd ee', '(\\w)\\1', 1       | [a, b, e]    |
        | empty matches follow Java    | 'abc', r'b*', 0                   | [, b, , ]    |

  Rule: regexp_replace

    Scenario Outline: regexp_replace: <case>
      When query
        """
        SELECT regexp_replace(<args>) AS result
        """
      Then query result
        | result   |
        | <result> |

      Examples:
        | case                              | args                                              | result            |
        | look-ahead                        | 'foo.bar.baz', '\\.(?=baz)', '_'                  | foo.bar_baz       |
        | leading zeros before a digit      | '00123', '^0+(?=\\d)', ''                         | 123               |
        | keeps the last zero               | '0000', '^0+(?=\\d)', ''                          | 0                 |
        | zero-width look-around            | 'a1b22c', '(?<=\\d)(?=\\D)', '-'                   | a1-b22-c          |
        | back-reference with group ref     | 'aabbcd', '(\\w)\\1', '<$1>'                       | <a><b>cd          |
        | collapse repeated characters      | 'aaa  bbb', '(.)\\1+', '$1'                       | a b               |
        | negative look-behind              | 'foo123', '(?<![0-9])\\d', 'X'                    | fooX23            |
        | negative look-ahead               | 'ПРОФСОЮЗ ПРОФСОЮЗНЫЙ', r'ПРОФСОЮЗ(?!НЫЙ)', 'X'   | X ПРОФСОЮЗНЫЙ     |
        | camel case boundary               | 'camelCaseString', r'(?<=[a-z])(?=[A-Z])', '_'    | camel_Case_String |
        | empty matches follow Java         | 'abc', 'b*', '-'                                  | -a--c-            |
        | group number followed by a digit  | 'b', '(b)', '$1a'                                 | ba                |
        | group number past the last group  | 'b', '(b)', '$10'                                 | b0                |
        | escaped dollar                    | 'a.b', '\\.', '\\$'                               | a$b               |
        | group references on the fast path | '2024-05-06', '(\\d+)-(\\d+)-(\\d+)', '$3.$2.$1'    | 06.05.2024        |
        | NULL input                        | CAST(NULL AS STRING), r'a(?=b)', 'x'              | NULL              |

  Rule: split

    Scenario Outline: split: <case>
      When query
        """
        SELECT split(<args>) AS result
        """
      Then query result
        | result   |
        | <result> |

      Examples:
        | case                              | args                                                 | result                  |
        | look-behind                       | 'ab,cd,ef', r'(?<=,)'                                | [ab,, cd,, ef]          |
        | look-ahead camel case             | 'camelCaseString', r'(?=[A-Z])'                      | [camel, Case, String]   |
        | look-behind and look-ahead        | 'ИванИванов', r'(?<=[а-я])(?=[А-Я])'                 | [Иван, Иванов]          |
        | negative look-behind              | 'a1b2c3', r'(?<![0-9])(?=[0-9])'                     | [a, 1b, 2c, 3]          |
        | back-reference                    | 'abXXcdYYef', '(X)\\1'                                | [ab, cdYYef]            |
        | zero-width match at the start     | 'abc', r'(?=a)'                                      | [abc]                   |
        | limit keeps the remainder         | 'a,b,c,d', ',', 2                                    | [a, b,c,d]              |
        | limit 0 keeps trailing empties    | 'a,b,c,,', ',', 0                                    | [a, b, c, , ]           |
        | empty pattern splits characters   | 'abc', ''                                            | [a, b, c]               |
        | empty pattern with a limit        | 'abc', '', 2                                         | [a, b]                  |
        | leading delimiter                 | ',a,b', ','                                          | [, a, b]                |
        | NULL input                        | CAST(NULL AS STRING), r'(?<=,)'                      | NULL                    |

  Rule: Class escapes are ASCII-only as in Java

    Scenario Outline: <case>
      When query
        """
        SELECT <expr> AS result
        """
      Then query result
        | result   |
        | <result> |

      Examples:
        | case                           | expr                                         | result        |
        | \\w does not match Cyrillic     | 'привет' RLIKE '^\\w+$'                      | false         |
        | \\w matches ASCII               | 'hello' RLIKE '^\\w+$'                       | true          |
        | \\w replaces ASCII words only   | regexp_replace('привет мир abc', '\\w+', 'X') | привет мир X |
        | \\d does not match Arabic digit | regexp_replace('a٣b4', '\\d', 'X')           | a٣bX          |
        | \\b is ASCII-only               | regexp_replace('привет abc', '\\b', '#')     | привет #abc#   |

  Rule: Column patterns

    Scenario: A pattern given as a column is compiled per distinct value
      When query
        """
        SELECT v, v RLIKE p AS result
        FROM VALUES ('aa', r'(\w)\1'), ('ab', r'(\w)\1'), ('ab', r'a(?=b)'), ('ab', r'b$') AS t(v, p)
        ORDER BY v, p
        """
      Then query result ordered
        | v  | result |
        | aa | true   |
        | ab | false  |
        | ab | true   |
        | ab | true   |

  Rule: Flag groups follow Java

    Scenario Outline: <case>
      When query
        """
        SELECT <expr> AS result
        """
      Then query result
        | result   |
        | <result> |

      Examples:
        | case                                   | expr                                                         | result  |
        | (?U) is not swap-greed                 | regexp_extract('aaa', '(?U)a+', 0)                           | aaa     |
        | (?U) with a lazy quantifier            | regexp_extract('aaa', '(?U)a+?', 0)                          | a       |
        | (?U) makes \\w Unicode                | 'привет' RLIKE '(?U)^\\w+$'                                | true    |
        | \\w is ASCII without it               | 'привет' RLIKE '^\\w+$'                                    | false   |
        | (?U) makes \\s Unicode                | regexp_replace(concat('a', char(160), 'b'), '(?U)\\s', '_')                 | a_b     |
        | (?U) scope ends with the group         | 'xя' RLIKE '(?U)x(?-U)\\w'                                 | false   |
        | (?U) mid-pattern                       | 'xя' RLIKE 'x(?U)\\w'                                      | true    |
        | (?U) with a back-reference             | 'ул. ул. Ленина' RLIKE '(?U)\\b(ул\\.?)\\s*\\1'     | true    |
        | (?i) folds ASCII only                  | 'ПРИВЕТ' RLIKE '(?i)привет'                                  | false   |
        | (?iu) folds Unicode                    | 'ПРИВЕТ' RLIKE '(?iu)привет'                                 | true    |
        | (?iU) folds Unicode                    | 'ПРИВЕТ' RLIKE '(?iU)привет'                                 | true    |
        | (?i) with ASCII letters                | 'ABC' RLIKE '(?i)[a-c]+'                                     | true    |
        | $ before a final newline               | concat('abc', char(10)) RLIKE 'c$'                                          | true    |
        | $ before a final CRLF                  | concat('abc', char(13), char(10)) RLIKE 'c$'                                       | true    |
        | $ not between CR and LF                | concat('abc', char(13), char(10)) RLIKE 'c\\r$'                                  | false   |
        | $ not before two newlines              | concat('abc', char(10), char(10)) RLIKE 'c$'                                       | false   |
