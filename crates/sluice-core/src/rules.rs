//! The rule set: budgets, what they match, and their limits.
//!
//! The extension only understands JSON (the `sluice.rules` GUC); the CLI turns
//! YAML into this JSON. [`validate`] is the single gatekeeper for both, so every
//! error carries a path such as `budgets[1].cpu_share` that a person can act on.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::time::Duration;

use jiff::Timestamp;
use serde::Deserialize;

use crate::tags::{MAX_KEY_LEN, MAX_VALUE_LEN};

pub const DEFAULT_MAX_BUDGETS: usize = 64;
pub const MAX_MATCHES: usize = 8;
pub const MAX_MATCH_KEYS: usize = 8;
pub const MAX_NAME_LEN: usize = 32;
pub const MAX_JSON_LEN: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct Rules {
    pub budgets: Vec<Budget>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Budget {
    pub name: String,
    pub mode: Mode,
    /// A query matches the budget if it matches any entry (OR); it matches an
    /// entry if it has every tag in it (AND).
    pub matches: Vec<BTreeMap<String, String>>,
    pub max_concurrent: Option<u32>,
    /// Raw planner cost units, comparable with `EXPLAIN`.
    pub max_query_cost: Option<f64>,
    pub cpu: Option<CpuLimit>,
    pub max_query_time: Option<Duration>,
    pub expires_at: Option<Timestamp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Off,
    Warn,
    Enforce,
}

/// Token bucket limits. `cpu_share` and `burst_s` only make sense together.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuLimit {
    /// Fraction of the server's CPU, in (0, 1].
    pub cpu_share: f64,
    /// Maximum debt, in seconds of the whole server.
    pub burst_s: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleError {
    /// Where the problem is, e.g. `budgets[1].cpu_share`. Empty for the document
    /// as a whole.
    pub path: String,
    pub message: String,
}

impl RuleError {
    fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for RuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(f, "{}: {}", self.path, self.message)
        }
    }
}

impl std::error::Error for RuleError {}

/// Parses and checks a JSON rule set. Returns every problem found, not just the
/// first, so one `sluice validate` run is enough to fix a file.
///
/// `max_budgets` is the `sluice.max_budgets` setting of the server the rules are
/// meant for; use [`DEFAULT_MAX_BUDGETS`] when it isn't known.
pub fn validate(json: &str, max_budgets: usize) -> Result<Rules, Vec<RuleError>> {
    if json.len() > MAX_JSON_LEN {
        return Err(vec![RuleError::new(
            "",
            format!("rules are {} bytes, limit is {MAX_JSON_LEN}", json.len()),
        )]);
    }

    let mut de = serde_json::Deserializer::from_str(json);
    let raw: RawRules = serde_path_to_error::deserialize(&mut de).map_err(|e| {
        let path = e.path().to_string();
        let path = if path == "." { String::new() } else { path };
        vec![RuleError::new(path, e.into_inner().to_string())]
    })?;
    if let Err(e) = de.end() {
        return Err(vec![RuleError::new("", e.to_string())]);
    }

    let mut errors = Vec::new();
    if let Some(v) = raw.version
        && v != 1
    {
        errors.push(RuleError::new(
            "version",
            format!("unsupported version {v}, expected 1"),
        ));
    }
    if raw.budgets.len() > max_budgets {
        errors.push(RuleError::new(
            "budgets",
            format!("{} budgets, server allows {max_budgets}", raw.budgets.len()),
        ));
    }

    let mut seen = HashSet::new();
    let mut budgets = Vec::with_capacity(raw.budgets.len());
    for (i, b) in raw.budgets.into_iter().enumerate() {
        let at = |field: &str| format!("budgets[{i}].{field}");
        let before = errors.len();

        if !valid_name(&b.name) {
            errors.push(RuleError::new(
                at("name"),
                format!(
                    "{:?} must be 1-{MAX_NAME_LEN} chars of a-z, 0-9, _ or -",
                    b.name
                ),
            ));
        } else if !seen.insert(b.name.clone()) {
            errors.push(RuleError::new(
                at("name"),
                format!("duplicate budget name {:?}", b.name),
            ));
        }

        check_matches(&b.matches, &at("match"), &mut errors);

        if let Some(cost) = b.max_query_cost
            && !(cost.is_finite() && cost > 0.0)
        {
            errors.push(RuleError::new(
                at("max_query_cost"),
                "must be greater than 0",
            ));
        }
        if let Some(share) = b.cpu_share
            && !(share > 0.0 && share <= 1.0)
        {
            errors.push(RuleError::new(at("cpu_share"), "must be in (0, 1]"));
        }
        if let Some(burst) = b.burst_s
            && !(burst.is_finite() && burst > 0.0)
        {
            errors.push(RuleError::new(at("burst_s"), "must be greater than 0"));
        }
        match (b.cpu_share, b.burst_s) {
            (Some(_), None) => errors.push(RuleError::new(
                at("burst_s"),
                "required when cpu_share is set",
            )),
            (None, Some(_)) => errors.push(RuleError::new(
                at("cpu_share"),
                "required when burst_s is set",
            )),
            _ => {}
        }
        if b.max_query_time_ms == Some(0) {
            errors.push(RuleError::new(
                at("max_query_time_ms"),
                "must be greater than 0",
            ));
        }

        if errors.len() == before {
            budgets.push(Budget {
                name: b.name,
                mode: b.mode,
                matches: b.matches,
                max_concurrent: b.max_concurrent,
                max_query_cost: b.max_query_cost,
                cpu: b
                    .cpu_share
                    .zip(b.burst_s)
                    .map(|(cpu_share, burst_s)| CpuLimit { cpu_share, burst_s }),
                max_query_time: b.max_query_time_ms.map(Duration::from_millis),
                expires_at: b.expires_at,
            });
        }
    }

    if errors.is_empty() {
        Ok(Rules { budgets })
    } else {
        Err(errors)
    }
}

fn valid_name(name: &str) -> bool {
    (1..=MAX_NAME_LEN).contains(&name.len())
        && name
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
}

fn check_matches(matches: &[BTreeMap<String, String>], at: &str, errors: &mut Vec<RuleError>) {
    if matches.is_empty() {
        errors.push(RuleError::new(at, "needs at least one entry"));
    }
    if matches.len() > MAX_MATCHES {
        errors.push(RuleError::new(
            at,
            format!("{} entries, limit is {MAX_MATCHES}", matches.len()),
        ));
    }
    for (j, m) in matches.iter().enumerate() {
        let at = format!("{at}[{j}]");
        // An empty entry would match every query.
        if m.is_empty() {
            errors.push(RuleError::new(&at, "needs at least one tag"));
        }
        if m.len() > MAX_MATCH_KEYS {
            errors.push(RuleError::new(
                &at,
                format!("{} tags, limit is {MAX_MATCH_KEYS}", m.len()),
            ));
        }
        // Tags longer than this are cut when read, so such a rule could never match.
        for (key, value) in m {
            if key.is_empty() || key.len() > MAX_KEY_LEN {
                errors.push(RuleError::new(
                    format!("{at}.{key}"),
                    format!("key must be 1-{MAX_KEY_LEN} bytes"),
                ));
            }
            if value.len() > MAX_VALUE_LEN {
                errors.push(RuleError::new(
                    format!("{at}.{key}"),
                    format!("value must be at most {MAX_VALUE_LEN} bytes"),
                ));
            }
        }
    }
}

/// The JSON as written; [`validate`] turns it into [`Rules`].
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRules {
    version: Option<u32>,
    budgets: Vec<RawBudget>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBudget {
    name: String,
    mode: Mode,
    #[serde(rename = "match")]
    matches: Vec<BTreeMap<String, String>>,
    max_concurrent: Option<u32>,
    max_query_cost: Option<f64>,
    cpu_share: Option<f64>,
    burst_s: Option<f64>,
    max_query_time_ms: Option<u64>,
    expires_at: Option<Timestamp>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(json: &str) -> Rules {
        validate(json, DEFAULT_MAX_BUDGETS).unwrap_or_else(|e| panic!("{e:?}"))
    }

    /// The error strings, as `path: message`.
    fn errors(json: &str) -> Vec<String> {
        validate(json, DEFAULT_MAX_BUDGETS)
            .expect_err("should be rejected")
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    fn budget(fields: &str) -> String {
        format!(r#"{{"budgets":[{{"name":"b","mode":"warn","match":[{{"app":"x"}}]{fields}}}]}}"#)
    }

    #[test]
    fn parses_full_budget() {
        let rules = ok(r#"{
            "version": 1,
            "budgets": [{
                "name": "dashboard",
                "mode": "enforce",
                "match": [{"app": "dashboard"}, {"feature": "export_csv", "priority": "low"}],
                "max_concurrent": 3,
                "max_query_cost": 5000000,
                "cpu_share": 0.25,
                "burst_s": 20,
                "max_query_time_ms": 30000,
                "expires_at": "2026-10-13T09:00:00+07:00"
            }]
        }"#);
        let b = &rules.budgets[0];
        assert_eq!(b.name, "dashboard");
        assert_eq!(b.mode, Mode::Enforce);
        assert_eq!(b.matches.len(), 2);
        assert_eq!(b.matches[1]["priority"], "low");
        assert_eq!(b.max_concurrent, Some(3));
        assert_eq!(b.max_query_cost, Some(5e6));
        assert_eq!(
            b.cpu,
            Some(CpuLimit {
                cpu_share: 0.25,
                burst_s: 20.0
            })
        );
        assert_eq!(b.max_query_time, Some(Duration::from_secs(30)));
        assert_eq!(b.expires_at, Some("2026-10-13T02:00:00Z".parse().unwrap()));
    }

    #[test]
    fn minimal_documents() {
        assert_eq!(ok(r#"{"budgets":[]}"#).budgets, vec![]);
        let b = &ok(&budget("")).budgets[0];
        assert_eq!(b.mode, Mode::Warn);
        assert_eq!(b.max_concurrent, None);
        assert_eq!(b.cpu, None);
        assert_eq!(b.expires_at, None);
    }

    #[test]
    fn max_concurrent_zero_blocks_everything() {
        assert_eq!(
            ok(&budget(r#","max_concurrent":0"#)).budgets[0].max_concurrent,
            Some(0)
        );
    }

    #[test]
    fn rejects_bad_json() {
        assert_eq!(errors("").len(), 1);
        assert_eq!(errors("{").len(), 1);
        assert_eq!(errors(r#"{"budgets":[]} x"#).len(), 1);
    }

    #[test]
    fn type_errors_have_a_path() {
        let e = errors(&budget(r#","cpu_share":"25%","burst_s":20"#));
        assert_eq!(e.len(), 1);
        assert!(e[0].starts_with("budgets[0].cpu_share: "), "{e:?}");

        let e = errors(&budget(r#","mode":"block""#).replace(r#""mode":"warn","#, ""));
        assert!(e[0].starts_with("budgets[0].mode: "), "{e:?}");

        let e = errors(&budget(r#","max_concurrent":-1"#));
        assert!(e[0].starts_with("budgets[0].max_concurrent: "), "{e:?}");
    }

    #[test]
    fn rejects_unknown_and_missing_fields() {
        let e = errors(&budget(r#","max_concurent":3"#));
        assert!(e[0].contains("max_concurent"), "{e:?}");

        let e = errors(r#"{"budgets":[{"name":"b","match":[{"app":"x"}]}]}"#);
        assert!(e[0].contains("mode"), "{e:?}");

        assert!(errors(r#"{"version":1}"#)[0].contains("budgets"));
    }

    #[test]
    fn rejects_unsupported_version() {
        assert_eq!(
            errors(r#"{"version":2,"budgets":[]}"#),
            ["version: unsupported version 2, expected 1"]
        );
    }

    #[test]
    fn checks_names() {
        for name in ["", "Dashboard", "a b", "dash.board", &"x".repeat(33)] {
            let json = budget("").replace(r#""name":"b""#, &format!(r#""name":{name:?}"#));
            let e = errors(&json);
            assert!(e[0].starts_with("budgets[0].name: "), "{name:?}: {e:?}");
        }
        ok(&budget("").replace(r#""name":"b""#, r#""name":"tmp-q_42""#));
        ok(&budget("").replace(r#""name":"b""#, &format!(r#""name":"{}""#, "x".repeat(32))));
    }

    #[test]
    fn rejects_duplicate_names() {
        let json = r#"{"budgets":[
            {"name":"a","mode":"warn","match":[{"app":"x"}]},
            {"name":"b","mode":"warn","match":[{"app":"y"}]},
            {"name":"a","mode":"off","match":[{"app":"z"}]}
        ]}"#;
        assert_eq!(
            errors(json),
            [r#"budgets[2].name: duplicate budget name "a""#]
        );
    }

    #[test]
    fn cpu_share_and_burst_come_together() {
        assert_eq!(
            errors(&budget(r#","cpu_share":0.25"#)),
            ["budgets[0].burst_s: required when cpu_share is set"]
        );
        assert_eq!(
            errors(&budget(r#","burst_s":20"#)),
            ["budgets[0].cpu_share: required when burst_s is set"]
        );
    }

    #[test]
    fn checks_number_ranges() {
        for (fields, path) in [
            (r#","cpu_share":0,"burst_s":1"#, "cpu_share"),
            (r#","cpu_share":1.5,"burst_s":1"#, "cpu_share"),
            (r#","cpu_share":-0.1,"burst_s":1"#, "cpu_share"),
            (r#","cpu_share":0.5,"burst_s":0"#, "burst_s"),
            (r#","max_query_cost":0"#, "max_query_cost"),
            (r#","max_query_cost":-5"#, "max_query_cost"),
            (r#","max_query_time_ms":0"#, "max_query_time_ms"),
        ] {
            let e = errors(&budget(fields));
            assert_eq!(e.len(), 1, "{fields}: {e:?}");
            assert!(
                e[0].starts_with(&format!("budgets[0].{path}: ")),
                "{fields}: {e:?}"
            );
        }
        ok(&budget(r#","cpu_share":1,"burst_s":0.5"#));
    }

    #[test]
    fn checks_matches() {
        let with_match = |m: &str| budget("").replace(r#"[{"app":"x"}]"#, m);

        assert_eq!(
            errors(&with_match("[]")),
            ["budgets[0].match: needs at least one entry"]
        );
        assert_eq!(
            errors(&with_match("[{}]")),
            ["budgets[0].match[0]: needs at least one tag"]
        );

        let nine: Vec<String> = (0..9).map(|i| format!(r#"{{"app":"a{i}"}}"#)).collect();
        assert_eq!(
            errors(&with_match(&format!("[{}]", nine.join(",")))),
            ["budgets[0].match: 9 entries, limit is 8"]
        );

        let keys: Vec<String> = (0..9).map(|i| format!(r#""k{i}":"v""#)).collect();
        assert_eq!(
            errors(&with_match(&format!("[{{{}}}]", keys.join(",")))),
            ["budgets[0].match[0]: 9 tags, limit is 8"]
        );

        let long_value = format!(r#"[{{"app":"{}"}}]"#, "v".repeat(65));
        assert_eq!(
            errors(&with_match(&long_value)),
            ["budgets[0].match[0].app: value must be at most 64 bytes"]
        );
        let long_key = format!(r#"[{{"{}":"v"}}]"#, "k".repeat(33));
        assert!(errors(&with_match(&long_key))[0].ends_with("key must be 1-32 bytes"));
    }

    #[test]
    fn checks_expires_at() {
        let e = errors(&budget(r#","expires_at":"tomorrow""#));
        assert!(e[0].starts_with("budgets[0].expires_at: "), "{e:?}");
        assert_eq!(
            ok(&budget(r#","expires_at":null"#)).budgets[0].expires_at,
            None
        );
    }

    #[test]
    fn enforces_size_limits() {
        let budgets: Vec<String> = (0..3)
            .map(|i| format!(r#"{{"name":"b{i}","mode":"warn","match":[{{"app":"x"}}]}}"#))
            .collect();
        let json = format!(r#"{{"budgets":[{}]}}"#, budgets.join(","));
        assert_eq!(
            validate(&json, 2).unwrap_err()[0].to_string(),
            "budgets: 3 budgets, server allows 2"
        );
        assert!(validate(&json, 3).is_ok());

        let huge = format!(r#"{{"budgets":[]{}}}"#, " ".repeat(MAX_JSON_LEN));
        assert!(errors(&huge)[0].contains("limit is 65536"));
    }

    #[test]
    fn reports_every_problem_at_once() {
        let json = r#"{"budgets":[
            {"name":"A","mode":"warn","match":[{"app":"x"}],"cpu_share":2},
            {"name":"ok","mode":"warn","match":[]}
        ]}"#;
        assert_eq!(
            errors(json),
            [
                r#"budgets[0].name: "A" must be 1-32 chars of a-z, 0-9, _ or -"#,
                "budgets[0].cpu_share: must be in (0, 1]",
                "budgets[0].burst_s: required when cpu_share is set",
                "budgets[1].match: needs at least one entry",
            ]
        );
    }
}
