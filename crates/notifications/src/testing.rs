//! A source-text reader for tests that assert on the shape of a statement.
//!
//! ## Why this exists
//!
//! Several of this crate's guarantees are not behavioural and cannot be made behavioural: a
//! sweep must read the **settle** instant rather than the enqueue instant, a predicate must
//! carry no status list, a work list must keep the orgless platform arm. In every case the two
//! implementations — right and wrong — answer the same query the same way, so a test that
//! drives the function proves nothing about the text.
//!
//! ## What it strips, and why that is not the obvious choice
//!
//! **Comments, and only comments.** The first version of this reader stripped string literals
//! as well, on the argument that a gate scanning the file it lives in will find its own
//! explanation. That argument is right about comments and **wrong about literals**, and the
//! four assertions it broke were the four that matter: the SQL lives in a `"..."` literal, so
//! stripping literals strips the very thing under test and the assertion could never pass —
//! which is worse than no assertion, because it looks like a failing gate rather than an
//! impossible one.
//!
//! Comments are the half that genuinely must go, and this crate's own docs are the reason. Every
//! fix here records the statement it replaced — the module header quotes
//! `status in ('sent', 'skipped')` and `created_at` on purpose, to say what changed — so a
//! reader that scanned raw text would find this file *documenting* the defect and read it as
//! the defect being present.
//!
//! ## What it cannot do
//!
//! It cannot tell whether a statement is *correct*. A test reading this crate's own source
//! proves the text says what the comment claims, and that is a real but narrow property. The
//! behavioural half is `scripts/qa/run-notification-retention.sh`, which runs the sweep against
//! a real PostgreSQL with real rows and reads the deletions back.

/// This file's code with its **comments** removed and its string literals kept.
///
/// Line structure is preserved, so a caller can talk about line numbers and about "the
/// statement above" without the offsets moving.
#[must_use]
pub fn strip_comments(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len());
    let mut index = 0_usize;

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum State {
        Code,
        /// `// …` to the end of the line.
        LineComment,
        /// `/* … */`, which unlike a line comment **spans** lines and must keep consuming
        /// newlines — that is what preserves the line count.
        BlockComment { depth: usize },
        /// Inside `"…"`. Kept verbatim in the output, because SQL is the subject here.
        Str { raw: bool, hashes: usize },
        Char,
    }
    let mut state = State::Code;

    while index < chars.len() {
        let current = chars[index];
        let next = chars.get(index + 1).copied();

        match state {
            State::LineComment => {
                if current == '\n' {
                    state = State::Code;
                    out.push('\n');
                }
                index += 1;
            }
            State::BlockComment { depth } => {
                if current == '\n' {
                    // Kept, so "line 40" means the same thing before and after.
                    out.push('\n');
                }
                if current == '/' && next == Some('*') {
                    // Nested: `/* /* */ */` is legal Rust, and a reader that does not COUNT
                    // closes on the inner `*/` and then reads the rest of the file as code.
                    state = State::BlockComment {
                        depth: depth.saturating_add(1),
                    };
                    index += 2;
                    continue;
                }
                if current == '*' && next == Some('/') {
                    index += 2;
                    // Only the LAST `*/` ends the comment.
                    state = if depth <= 1 {
                        State::Code
                    } else {
                        State::BlockComment { depth: depth - 1 }
                    };
                    continue;
                }
                index += 1;
            }
            State::Char => {
                if current == '\\' {
                    // Skip the escaped character so `'\''` does not end the literal.
                    index += 2;
                } else {
                    index += 1;
                    state = State::Code;
                }
            }
            State::Str { raw, hashes } => {
                if raw {
                    // Terminates at `"` followed by exactly `hashes` more `#`. Fewer than
                    // `hashes` means this `"` is inside the literal.
                    if current == '"' {
                        let mut ahead = index + 1;
                        let mut seen = 0_usize;
                        while seen < hashes && chars.get(ahead) == Some(&'#') {
                            ahead += 1;
                            seen += 1;
                        }
                        if seen == hashes {
                            out.push('"');
                            for _ in 0..hashes {
                                out.push('#');
                            }
                            index = ahead + 1;
                            state = State::Code;
                            continue;
                        }
                    }
                    out.push(current);
                    index += 1;
                } else if current == '\\' {
                    // The escape AND the character it escapes, verbatim.
                    out.push(current);
                    if let Some(escaped) = next {
                        out.push(escaped);
                    }
                    index += 2;
                } else {
                    if current == '"' {
                        state = State::Code;
                    } else if current == '\n' {
                        // An unterminated literal would otherwise swallow the rest of the file
                        // — and a file with a stray quote is exactly what this reader gets fed
                        // when a half-written edit is tested.
                        state = State::Code;
                        out.push('\n');
                        index += 1;
                        continue;
                    }
                    out.push(current);
                    index += 1;
                }
            }
            State::Code => {
                match (current, next) {
                    ('/', Some('/')) => {
                        state = State::LineComment;
                        index += 2;
                    }
                    ('/', Some('*')) => {
                        state = State::BlockComment { depth: 1 };
                        index += 2;
                    }
                    ('"', _) => {
                        // `r"…"`, `r#"…"#` and `br#"…"#` all open a raw string.
                        let mut probe = index;
                        let is_b = chars.get(probe) == Some(&'b');
                        if is_b {
                            probe += 1;
                        }
                        if chars.get(probe) == Some(&'r') {
                            probe += 1;
                            let mut hashes = 0_usize;
                            while chars.get(probe) == Some(&'#') {
                                hashes += 1;
                                probe += 1;
                            }
                            if chars.get(probe) == Some(&'"') {
                                state = State::Str {
                                    raw: true,
                                    hashes,
                                };
                                index = probe + 1;
                                continue;
                            }
                        }
                        state = State::Str {
                            raw: false,
                            hashes: 0,
                        };
                        out.push('"');
                        index += 1;
                    }
                    ('\'', _) => {
                        // A character literal is `'a'` or an escape; a **lifetime** is `'a` with
                        // no closing quote on the character. Treating `&'a str` as a literal
                        // swallows the rest of the file, which is the failure this arm exists
                        // to prevent — it has bitten every hand-written scanner of Rust source.
                        let closes = next == Some('\\') || chars.get(index + 2) == Some(&'\'');
                        if closes {
                            state = State::Char;
                            index += 1;
                        } else {
                            out.push(current);
                            index += 1;
                        }
                    }
                    _ => {
                        out.push(current);
                        index += 1;
                    }
                }
            }
        }
    }

    out
}

/// This file's code with comments removed **and the `#[cfg(test)]` module dropped**.
///
/// The test module is dropped for a reason this branch learned the hard way: a gate that
/// searches a file for a string will find its own assertion, because the assertion's argument
/// is a string literal in that same file. The search string of `the_sweep_judges_a_row_by_when
/// _it_was_settled` is `coalesce(…)` — and with literals kept, the test module alone would
/// satisfy it. Truncating at `#[cfg(test)]` keeps the property under test in the file and the
/// evidence for it out of the evidence.
///
/// The module header (`//! …`) is a comment and is therefore already gone, which is what stops
/// this file's own "the old statement said `status in ('sent', 'skipped')`" from being read as
/// the old statement being present.
#[must_use]
pub fn code_of(source: &str) -> String {
    let stripped = strip_comments(source);
    match stripped.find("#[cfg(test)]") {
        Some(at) => stripped[..at].to_owned(),
        None => stripped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_comment_is_removed_and_keeps_its_line() {
        let stripped = strip_comments("let a = 1; // this is gone\nlet b = 2;");
        assert!(!stripped.contains("gone"));
        assert!(stripped.contains("let a = 1;"));
        assert!(stripped.contains("let b = 2;"));
        assert_eq!(stripped.lines().count(), 2);
    }

    /// **A block comment spans lines, and the first version of this reader treated `/*` as a
    /// line comment** — so `/* one \n two \n three */` leaked `two` and `three` into the output
    /// and every assertion below it saw text that was not code. Caught by its own test, which
    /// is the only place a hand-written scanner's first failure shows up.
    #[test]
    fn a_block_comment_is_removed_whole_and_keeps_the_line_count() {
        let stripped = strip_comments("a;\n/* one\ntwo\nthree */\nb;");
        assert_eq!(stripped.lines().count(), 5, "the line count must survive");
        assert!(!stripped.contains("one"));
        assert!(!stripped.contains("two"));
        assert!(!stripped.contains("three"));
        assert!(stripped.contains("a;"));
        assert!(stripped.contains("b;"));
    }

    /// Nested block comments are legal Rust, and a non-counting reader leaves the rest of the
    /// file inside a comment — which looks like "the file has no code at all".
    #[test]
    fn a_nested_block_comment_closes_on_its_own_end() {
        let stripped = strip_comments("/* outer /* inner */ still outer */ let after = 1;");
        assert!(!stripped.contains("still outer"));
        assert!(stripped.contains("let after = 1;"));
    }

    /// **String literals are KEPT, and this is the decision the reader exists to record.**
    ///
    /// The first version stripped them, on the argument that a gate must not find its own
    /// explanation. That argument is right about comments and wrong about literals: the SQL
    /// under test lives in a `"…"` literal, so stripping literals makes every assertion about
    /// a statement unsatisfiable — an impossible gate that reads as a failing one.
    #[test]
    fn a_string_literal_survives_because_the_sql_is_one() {
        let stripped = strip_comments("let q = \"select coalesce(a, b) from t\"; // gone");
        assert!(stripped.contains("select coalesce(a, b) from t"));
        assert!(!stripped.contains("gone"));
    }

    /// The gate-does-not-find-its-own-message property is now carried by `code_of`, which drops
    /// the test module — and this is the assertion for it.
    #[test]
    fn the_test_module_is_dropped_so_a_gate_cannot_find_its_own_assertion() {
        let source = "fn sweep() { let q = \"select 1\"; }\n#[cfg(test)]\nmod tests { \
                     fn t() { assert!(code().contains(\"select 1\")); } }\n";
        let code = code_of(source);
        assert!(code.contains("select 1"), "the production code stays");
        assert!(
            !code.contains("assert!"),
            "the test module must be dropped, or its assertion string satisfies its own gate"
        );
    }

    /// **A doc comment quoting the statement it replaced must not satisfy an assertion.**
    ///
    /// This is the whole reason `strip_comments` exists: this crate's module header names the
    /// old `status in ('sent', 'skipped')` predicate and the old `created_at` clock, to record
    /// what the fix changed. A reader that scanned raw text would find this file *documenting*
    /// the defect and read it as the defect being present.
    #[test]
    fn a_comment_quoting_the_old_statement_does_not_satisfy_an_assertion() {
        let source = "//! it used to select status in ('sent', 'skipped')\nlet clock = 1;";
        let stripped = strip_comments(source);
        assert!(!stripped.contains("status in ("));
        assert!(stripped.contains("let clock = 1;"));
    }

    /// **A raw string is a real shape in this codebase — SQL lives in one — and a reader that
    /// did not understand `r#"…"#` would end the literal at the first `"` inside it and treat
    /// the remainder of the file as string content.**
    #[test]
    fn a_raw_string_does_not_end_at_an_inner_quote() {
        let stripped = strip_comments("let q = r#\"select 'a' as x\"#;\nlet after = 2;");
        assert!(stripped.contains("let after = 2;"));
        assert!(stripped.contains("select 'a' as x"));
    }

    /// A lifetime is not a character literal. Getting this wrong turns every `&'a str` in a
    /// crate into an unterminated literal, which swallows the remainder of the file — and the
    /// symptom (a file with no readable code) looks like nothing at all rather than like a bug.
    #[test]
    fn a_lifetime_is_not_a_character_literal() {
        let stripped = strip_comments("fn f<'a>(x: &'a str) -> &'a str { \"kept\" }");
        assert!(stripped.contains("fn f"));
        assert!(stripped.contains("&'a str"), "the lifetimes must survive as code");
        assert!(stripped.contains("kept"));
    }

    /// A real escaped quote inside a literal must not end it early.
    #[test]
    fn an_escaped_quote_does_not_end_the_literal() {
        let stripped = strip_comments("let s = \"a \\\" b\";\nlet t = 3;");
        assert!(stripped.contains("let t = 3;"));
    }

    /// An unterminated literal must not swallow the rest of the file — which is what a
    /// half-written edit hands this reader.
    #[test]
    fn an_unterminated_literal_stops_at_the_line_end() {
        let stripped = strip_comments("let s = \"oops\nlet t = 3;");
        assert!(stripped.contains("let t = 3;"));
    }
}