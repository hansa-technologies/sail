@function(lambda)
Feature: zip_with higher-order function

  Rule: Element-wise combination of two arrays

    Scenario Outline: zip_with: <case>
      When query
        """
        SELECT zip_with(<left>, <right>, (x, y) -> <expr>) AS result
        """
      Then query result
        | result   |
        | <result> |

      Examples:
        | case                          | left                 | right                | expr           | result                       |
        | Sum of equal-length arrays    | array(1, 2, 3)       | array(4, 5, 6)       | x + y          | [5, 7, 9]                    |
        | Strings are concatenated      | array('a', 'b', 'c') | array('d', 'e', 'f') | concat(x, y)   | [ad, be, cf]                 |
        | Only the second parameter     | array(1, 2)          | array(7, 8, 9)       | y              | [7, 8, 9]                    |
        | Only the first parameter      | array(1, 2)          | array(7, 8, 9)       | x              | [1, 2, NULL]                 |
        | Shorter left array is padded  | array(1)             | array(3, 4, 5)       | coalesce(x, 0) + y | [4, 4, 5]                |
        | Shorter right array is padded | array(1, 2)          | array(3)             | x + y          | [4, NULL]                    |
        | Both arrays are empty         | array()              | array()              | x              | []                           |
        | Comparison with the previous  | array(1, 1, 2, 2, 3) | slice(array(1, 1, 2, 2, 3), 2, 5) | x != y | [false, true, false, true, NULL] |

  Rule: NULL arrays

    Scenario Outline: A NULL array makes the result NULL: <case>
      When query
        """
        SELECT zip_with(<left>, <right>, (x, y) -> x) AS result
        """
      Then query result
        | result |
        | NULL   |

      Examples:
        | case        | left                        | right                       |
        | NULL left   | CAST(NULL AS ARRAY<INT>)    | array(1)                    |
        | NULL right  | array(1)                    | CAST(NULL AS ARRAY<INT>)    |

  Rule: Result types and captured columns

    Scenario: The lambda may return a struct
      When query
        """
        SELECT zip_with(array(1, 2, 3), array('a', 'b', 'c'), (x, y) -> (y, x)) AS result
        """
      Then query result
        | result                  |
        | [{a, 1}, {b, 2}, {c, 3}] |

    Scenario: The lambda may capture columns of the row
      When query
        """
        SELECT zip_with(a, b, (x, y) -> x * k + coalesce(y, 0)) AS result
        FROM VALUES (array(1, 2), array(10, 20, 30), 2), (array(5), array(1), 3), (CAST(NULL AS ARRAY<INT>), array(1), 1) AS t(a, b, k)
        ORDER BY k DESC
        """
      Then query result ordered
        | result          |
        | [16]            |
        | [12, 24, NULL]  |
        | NULL            |

    Scenario: zip_with nests inside transform
      When query
        """
        SELECT transform(array(1, 2), v -> zip_with(array(v, v), array(10), (x, y) -> x + coalesce(y, 0))) AS result
        """
      Then query result
        | result               |
        | [[11, 1], [12, 2]]   |
