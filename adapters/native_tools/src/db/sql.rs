//! Reading SQL text just enough to enforce policy, without executing it.
//!
//! None of this is a substitute for the engine: reads run on a read-only connection and writes run
//! in a transaction that is rolled back if it changes too much. These checks exist to give clear
//! refusals early and to require a real `WHERE` on `UPDATE` and `DELETE`.

use regex::Regex;
use std::sync::OnceLock;

/// The statement with comments removed and the inside of string literals and quoted names blanked,
/// so keywords are only found where SQL would read them as keywords.
pub fn skeleton(sql: &str) -> String {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            '-' if next == Some('-') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if next == Some('*') => {
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i += 2;
                out.push(' ');
            }
            '\'' | '"' | '`' => {
                let q = c;
                out.push(q);
                i += 1;
                while i < chars.len() {
                    if chars[i] == q {
                        // A doubled quote is an escaped quote and stays inside the literal.
                        if chars.get(i + 1) == Some(&q) {
                            out.push(' ');
                            out.push(' ');
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    out.push(' ');
                    i += 1;
                }
                out.push(q);
                i += 1;
            }
            '[' => {
                out.push('[');
                i += 1;
                while i < chars.len() && chars[i] != ']' {
                    out.push(' ');
                    i += 1;
                }
                out.push(']');
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

pub fn first_keyword(skel: &str) -> String {
    skel.trim_start_matches(|c: char| c.is_whitespace() || c == ';' || c == '(')
        .split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn word(name: &'static str) -> Regex {
    Regex::new(&format!(r"(?i)\b{name}\b")).expect("regex compiles")
}

pub fn contains_word(skel: &str, name: &'static str) -> bool {
    static CACHE: OnceLock<std::sync::Mutex<std::collections::HashMap<&'static str, Regex>>> = OnceLock::new();
    let mut m = CACHE.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    m.entry(name).or_insert_with(|| word(name)).is_match(skel)
}

/// `WHERE` followed by something that is true for every row.
pub fn tautological_where(skel: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r#"(?i)\bwhere\s*\(*\s*(?:1\s*=\s*1|true|1|''\s*=\s*''|""\s*=\s*""|null\s+is\s+null|1\s*<>\s*0|0\s*=\s*0|2\s*>\s*1)\s*\)*\s*(?:;|$)"#).expect("regex compiles")
    });
    re.is_match(skel.trim_end())
}

/// True when text follows the first statement (after a `;`).
pub fn has_second_statement(skel: &str) -> bool {
    match skel.trim_end().trim_end_matches(';').find(';') {
        Some(_) => true,
        None => false,
    }
}

/// Words that mean a statement changes the schema or the database settings.
pub fn is_ddl_keyword(kw: &str) -> bool {
    matches!(kw, "create" | "drop" | "alter" | "reindex" | "vacuum" | "analyze" | "attach" | "detach" | "pragma" | "begin" | "commit" | "rollback" | "savepoint" | "release")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_and_literals_do_not_hide_or_fake_keywords() {
        let s = skeleton("SELECT 'a; DROP TABLE t' AS x -- drop table y\n FROM t /* delete from z */");
        assert!(!contains_word(&s, "drop") && !contains_word(&s, "delete"), "{s}");
        assert!(!has_second_statement(&s), "{s}");
        let s = skeleton("SELECT 1; DROP TABLE t");
        assert!(has_second_statement(&s) && contains_word(&s, "drop"));
        let s = skeleton(r#"SELECT "it""s ; here" FROM [odd ; name]"#);
        assert!(!has_second_statement(&s), "{s}");
        let s = skeleton("select 'it''s ; fine'");
        assert!(!has_second_statement(&s), "{s}");
    }

    #[test]
    fn the_first_keyword_ignores_leading_noise() {
        for (sql, kw) in [("SELECT 1", "select"), ("  \n with x as (select 1) select * from x", "with"), ("/* c */ insert into t values (1)", "insert"), ("(select 1)", "select"), ("; drop table t", "drop"), ("-- hi\nDELETE FROM t", "delete")] {
            assert_eq!(first_keyword(&skeleton(sql)), kw, "{sql}");
        }
    }

    #[test]
    fn where_clauses_that_match_every_row_are_recognised() {
        for sql in ["DELETE FROM t WHERE 1=1", "delete from t where 1 = 1;", "UPDATE t SET a=1 WHERE true", "DELETE FROM t WHERE (1)", "delete from t where ''=''", "DELETE FROM t WHERE 1", "delete from t where null is null", "update t set a=0 where 2>1"] {
            assert!(tautological_where(&skeleton(sql)), "{sql}");
        }
        for sql in ["DELETE FROM t WHERE id = 5", "DELETE FROM t WHERE id=1 OR name='x'", "update t set a=1 where a is not null", "DELETE FROM t WHERE 1=1 AND id=3"] {
            assert!(!tautological_where(&skeleton(sql)), "{sql}");
        }
        assert!(contains_word(&skeleton("delete from t where id=1"), "where"));
        assert!(!contains_word(&skeleton("delete from t"), "where"));
        assert!(!contains_word(&skeleton("delete from somewhere_table"), "where"));
    }

    #[test]
    fn schema_and_settings_statements_are_flagged() {
        for kw in ["create", "drop", "alter", "attach", "pragma", "vacuum", "begin", "commit"] {
            assert!(is_ddl_keyword(kw), "{kw}");
        }
        for kw in ["select", "insert", "update", "delete", "with"] {
            assert!(!is_ddl_keyword(kw), "{kw}");
        }
    }
}
