//! Tags read from the sqlcommenter comment at the end of a statement.
//!
//! Only a `/* ... */` comment at the very end of the statement counts; trailing
//! whitespace and semicolons after it are fine. A comment anywhere else, or a
//! trailing comment that is not well-formed sqlcommenter, yields no tags. When in
//! doubt the query simply matches no budget, which is the safe direction.
//!
//! Format: `/*key='value',key2='value2'*/`, keys and values URL-encoded, `'` inside
//! a value escaped as `\'`.

use std::sync::LazyLock;

use arrayvec::{ArrayString, ArrayVec};
use memchr::memmem;
use regex::Regex;

/// Only the last `MAX_SCAN` bytes of the statement are searched for the comment.
pub const MAX_SCAN: usize = 1024;
/// Tags beyond the first `MAX_TAGS` are dropped.
pub const MAX_TAGS: usize = 16;
/// Longer keys are cut to this many bytes (on a char boundary).
pub const MAX_KEY_LEN: usize = 32;
/// Longer values are cut to this many bytes (on a char boundary).
pub const MAX_VALUE_LEN: usize = 64;

/// Up to [`MAX_TAGS`] key/value pairs, stored inline so building one never
/// touches the heap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tags {
    pairs: ArrayVec<(ArrayString<MAX_KEY_LEN>, ArrayString<MAX_VALUE_LEN>), MAX_TAGS>,
}

impl Tags {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a tag, cutting key and value to their maximum length. If the key is
    /// already present the existing value is kept; if the set is full the tag is
    /// dropped.
    pub fn insert(&mut self, key: &str, value: &str) {
        let key = truncate(key, MAX_KEY_LEN);
        if self.pairs.is_full() || self.get(key).is_some() {
            return;
        }
        let mut k = ArrayString::new();
        k.push_str(key);
        let mut v = ArrayString::new();
        v.push_str(truncate(value, MAX_VALUE_LEN));
        self.pairs.push((k, v));
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k.as_str() == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.pairs.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub fn len(&self) -> usize {
        self.pairs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }
}

fn truncate(s: &str, max: usize) -> &str {
    &s[..s.floor_char_boundary(max)]
}

/// Reads the tags from the trailing sqlcommenter comment of `sql`.
pub fn parse_tags(sql: &str) -> Tags {
    trailing_comment(sql)
        .and_then(parse_body)
        .unwrap_or_default()
}

/// Returns the body of the `/* ... */` comment that ends the statement, if any.
///
/// Plain string search rather than a regex: a regex matches left to right, so
/// it would start from the first `/*` in the statement (which may sit inside a
/// string literal) and read the whole statement. Searching backwards from the
/// end lands on the right `/*` and only looks at the tail. `memmem` does that
/// search with SIMD, many bytes per instruction.
fn trailing_comment(sql: &str) -> Option<&str> {
    let sql = sql.trim_end_matches(|c: char| c.is_ascii_whitespace() || c == ';');
    let sql = sql.strip_suffix("*/")?;
    let window = sql.len().saturating_sub(MAX_SCAN);
    let open = window + memmem::rfind(&sql.as_bytes()[window..], b"/*")?;
    let body = &sql[open + 2..];
    // `/* a */ b */`: the statement does not end in a single comment.
    if memmem::find(body.as_bytes(), b"*/").is_some() {
        return None;
    }
    Some(body)
}

/// One `key='value'` pair. The value may contain `\'` (or any `\x` escape).
const PAIR: &str = r"([^\s=',]+)\s*=\s*'((?:[^'\\]|\\.)*)'";

static PAIR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(PAIR).unwrap());

/// A whole comment body: nothing, or pairs separated by commas.
static BODY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"(?s)^\s*(?:{PAIR}\s*(?:,\s*{PAIR}\s*)*)?$")).unwrap());

fn parse_body(body: &str) -> Option<Tags> {
    if !BODY_RE.is_match(body) {
        return None;
    }
    // The body is known to be well-formed, so the pairs are exactly the
    // non-overlapping matches of PAIR, left to right.
    let mut tags = Tags::new();
    for caps in PAIR_RE.captures_iter(body) {
        tags.insert(&decode(&caps[1])?, &decode(&caps[2])?);
    }
    Some(tags)
}

/// Undoes `\x` escapes and `%XX` URL-encoding.
fn decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                out.push(*bytes.get(i + 1)?);
                i += 2;
            }
            b'%' => {
                let hex = bytes.get(i + 1..i + 3)?;
                out.push(hex_val(hex[0])? << 4 | hex_val(hex[1])?);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(sql: &str) -> Vec<(String, String)> {
        parse_tags(sql)
            .iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect()
    }

    fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
        expected
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn reads_trailing_comment() {
        assert_eq!(
            tags("SELECT * FROM orders /*app='dashboard',feature='export_csv'*/"),
            pairs(&[("app", "dashboard"), ("feature", "export_csv")])
        );
    }

    #[test]
    fn allows_semicolon_and_whitespace_after_comment() {
        assert_eq!(
            tags("SELECT 1 /*app='api'*/ ;\n  "),
            pairs(&[("app", "api")])
        );
    }

    #[test]
    fn tolerates_whitespace_inside_comment() {
        assert_eq!(
            tags("SELECT 1 /* app = 'api' , route='x' */"),
            pairs(&[("app", "api"), ("route", "x")])
        );
    }

    #[test]
    fn url_decodes_keys_and_values() {
        assert_eq!(
            tags("SELECT 1 /*route='%2Fapi%2Fusers%3Fpage%3D2',caf%C3%A9='na%C3%AFve'*/"),
            pairs(&[("route", "/api/users?page=2"), ("café", "naïve")])
        );
    }

    #[test]
    fn unescapes_quotes() {
        assert_eq!(
            tags(r"SELECT 1 /*note='it\'s'*/"),
            pairs(&[("note", "it's")])
        );
    }

    #[test]
    fn escaped_quote_and_comma_stay_inside_the_value() {
        assert_eq!(
            tags(r"SELECT 1 /*note='a\',b=\'c',app='x'*/"),
            pairs(&[("note", "a',b='c"), ("app", "x")])
        );
    }

    #[test]
    fn first_duplicate_key_wins() {
        assert_eq!(tags("SELECT 1 /*app='a',app='b'*/"), pairs(&[("app", "a")]));
    }

    #[test]
    fn empty_comment_has_no_tags() {
        assert!(parse_tags("SELECT 1 /**/").is_empty());
        assert!(parse_tags("SELECT 1 /*   */").is_empty());
    }

    #[test]
    fn ignores_comment_in_the_middle() {
        assert!(parse_tags("SELECT /*app='x'*/ 1").is_empty());
        assert!(parse_tags("/*app='x'*/ SELECT 1").is_empty());
    }

    #[test]
    fn ignores_statement_without_comment() {
        assert!(parse_tags("").is_empty());
        assert!(parse_tags("SELECT 1").is_empty());
        assert!(parse_tags("*/").is_empty());
    }

    #[test]
    fn ignores_unterminated_comment() {
        assert!(parse_tags("SELECT 1 /*app='x'").is_empty());
        assert!(parse_tags("SELECT 1 /*app='x'*").is_empty());
    }

    #[test]
    fn ignores_malformed_comment() {
        for sql in [
            "SELECT 1 /* just a note */",
            "SELECT 1 /*app=x*/",
            "SELECT 1 /*app='x*/",
            "SELECT 1 /*='x'*/",
            "SELECT 1 /*app='x' feature='y'*/",
            "SELECT 1 /*app='x',*/",
            "SELECT 1 /*a'pp='x'*/",
            "SELECT 1 /*app='%zz'*/",
            "SELECT 1 /*app='%4'*/",
            "SELECT 1 /*app='%FF'*/",
            r"SELECT 1 /*app='x\'*/",
        ] {
            assert!(parse_tags(sql).is_empty(), "{sql}");
        }
    }

    #[test]
    fn ignores_comment_markers_inside_string_literals() {
        assert!(parse_tags("SELECT '*/'").is_empty());
        assert!(parse_tags("SELECT 'a /* app=''x'' */'").is_empty());
        assert_eq!(
            tags("SELECT '/* app=''x'' */' /*app='y'*/"),
            pairs(&[("app", "y")])
        );
    }

    #[test]
    fn rejects_text_after_closed_comment() {
        assert!(parse_tags("SELECT 1 /*app='x'*/ + 1 */").is_empty());
    }

    #[test]
    fn only_scans_the_tail() {
        let long = "x".repeat(MAX_SCAN);
        assert!(parse_tags(&format!("SELECT 1 /*note='{long}'*/")).is_empty());

        let long_sql = format!("SELECT '{long}' /*app='x'*/");
        assert_eq!(tags(&long_sql), pairs(&[("app", "x")]));
    }

    #[test]
    fn scan_window_does_not_split_utf8() {
        // The window start lands inside multi-byte characters; must not panic.
        let sql = format!("SELECT '{}' /*app='x'*/", "é".repeat(MAX_SCAN));
        assert_eq!(tags(&sql), pairs(&[("app", "x")]));
    }

    #[test]
    fn keeps_only_the_first_max_tags() {
        let body: Vec<String> = (0..MAX_TAGS + 4).map(|i| format!("k{i}='v'")).collect();
        let parsed = parse_tags(&format!("SELECT 1 /*{}*/", body.join(",")));
        assert_eq!(parsed.len(), MAX_TAGS);
        assert_eq!(parsed.get("k15"), Some("v"));
        assert_eq!(parsed.get("k16"), None);
    }

    #[test]
    fn cuts_long_keys_and_values() {
        let key = "k".repeat(MAX_KEY_LEN + 10);
        let value = "v".repeat(MAX_VALUE_LEN + 10);
        let parsed = parse_tags(&format!("SELECT 1 /*{key}='{value}'*/"));
        assert_eq!(
            parsed.get(&key[..MAX_KEY_LEN]),
            Some(&value[..MAX_VALUE_LEN])
        );
    }

    #[test]
    fn cut_does_not_split_a_char() {
        // 63 ASCII bytes, then a 2-byte char that would end at byte 65.
        let prefix = "a".repeat(MAX_VALUE_LEN - 1);
        let parsed = parse_tags(&format!("SELECT 1 /*k='{prefix}%C3%A9'*/"));
        assert_eq!(parsed.get("k"), Some(prefix.as_str()));
    }

    #[test]
    fn bad_escape_after_the_cut_is_still_malformed() {
        let value = "v".repeat(MAX_VALUE_LEN);
        assert!(parse_tags(&format!("SELECT 1 /*k='{value}%zz'*/")).is_empty());
    }

    #[test]
    fn insert_cuts_on_char_boundary() {
        let mut tags = Tags::new();
        let value = "é".repeat(MAX_VALUE_LEN);
        tags.insert("k", &value);
        assert_eq!(tags.get("k"), Some(&value[..MAX_VALUE_LEN]));
    }
}
