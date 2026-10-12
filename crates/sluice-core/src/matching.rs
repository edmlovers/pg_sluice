//! Which budgets a query's tags fall under.
//!
//! Each match entry of each budget becomes one rule. A rule is indexed under
//! just one of its tags: a query that satisfies the rule must carry that tag,
//! so looking up the query's own tags finds every candidate, and the remaining
//! conditions are then checked one by one.

use std::hash::{Hash, Hasher};

use hashbrown::{Equivalent, HashMap};
use smallvec::SmallVec;

use crate::rules::Rules;
use crate::tags::Tags;

/// Position of a budget in [`Rules::budgets`], i.e. its declaration order.
pub type BudgetIdx = u16;

/// Budgets a query matched, in declaration order. Stays on the stack for up to
/// 8 budgets.
pub type Matched = SmallVec<[BudgetIdx; 8]>;

#[derive(Debug, Clone, Default)]
pub struct RuleIndex {
    by_tag: HashMap<(String, String), Vec<usize>>,
    rules: Vec<Rule>,
}

#[derive(Debug, Clone)]
struct Rule {
    budget: BudgetIdx,
    conditions: Vec<(String, String)>,
}

impl RuleIndex {
    /// # Panics
    ///
    /// If `rules` has more than `u16::MAX` budgets; [`crate::validate`] never
    /// lets that through.
    pub fn new(rules: &Rules) -> Self {
        let mut index = Self::default();
        for (i, budget) in rules.budgets.iter().enumerate() {
            let budget_idx = BudgetIdx::try_from(i).expect("too many budgets");
            for m in &budget.matches {
                let conditions: Vec<_> = m.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                let Some(first) = conditions.first().cloned() else {
                    continue;
                };
                index
                    .by_tag
                    .entry(first)
                    .or_default()
                    .push(index.rules.len());
                index.rules.push(Rule {
                    budget: budget_idx,
                    conditions,
                });
            }
        }
        index
    }

    /// Budgets whose rules `tags` satisfy, each once, in declaration order.
    pub fn matching_budgets(&self, tags: &Tags) -> Matched {
        let mut out = Matched::new();
        if self.rules.is_empty() {
            return out;
        }
        for (key, value) in tags.iter() {
            let Some(candidates) = self.by_tag.get(&TagRef(key, value)) else {
                continue;
            };
            for &r in candidates {
                let rule = &self.rules[r];
                if !out.contains(&rule.budget)
                    && rule
                        .conditions
                        .iter()
                        .all(|(k, v)| tags.get(k) == Some(v.as_str()))
                {
                    out.push(rule.budget);
                }
            }
        }
        out.sort_unstable();
        out
    }
}

/// Borrowed lookup key for `by_tag`, so a lookup doesn't build two `String`s.
/// Hashes exactly like `(String, String)`.
struct TagRef<'a>(&'a str, &'a str);

impl Hash for TagRef<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
        self.1.hash(state);
    }
}

impl Equivalent<(String, String)> for TagRef<'_> {
    fn equivalent(&self, key: &(String, String)) -> bool {
        self.0 == key.0 && self.1 == key.1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::{DEFAULT_MAX_BUDGETS, validate};

    /// Builds an index from `(name, match list as JSON)` pairs.
    fn index(budgets: &[(&str, &str)]) -> RuleIndex {
        let budgets: Vec<String> = budgets
            .iter()
            .map(|(name, matches)| {
                format!(r#"{{"name":"{name}","mode":"enforce","match":{matches}}}"#)
            })
            .collect();
        let json = format!(r#"{{"budgets":[{}]}}"#, budgets.join(","));
        RuleIndex::new(&validate(&json, DEFAULT_MAX_BUDGETS).unwrap())
    }

    fn tags(pairs: &[(&str, &str)]) -> Tags {
        let mut tags = Tags::new();
        for (k, v) in pairs {
            tags.insert(k, v);
        }
        tags
    }

    fn matched(index: &RuleIndex, pairs: &[(&str, &str)]) -> Vec<BudgetIdx> {
        index.matching_budgets(&tags(pairs)).to_vec()
    }

    #[test]
    fn single_tag() {
        let idx = index(&[("dash", r#"[{"app":"dashboard"}]"#)]);
        assert_eq!(matched(&idx, &[("app", "dashboard")]), [0]);
        assert!(matched(&idx, &[("app", "api")]).is_empty());
        assert!(matched(&idx, &[("user", "dashboard")]).is_empty());
        assert!(matched(&idx, &[]).is_empty());
    }

    #[test]
    fn extra_tags_do_not_matter() {
        let idx = index(&[("dash", r#"[{"app":"dashboard"}]"#)]);
        assert_eq!(
            matched(
                &idx,
                &[("user", "bob"), ("app", "dashboard"), ("route", "/x")]
            ),
            [0]
        );
    }

    #[test]
    fn values_are_case_sensitive() {
        let idx = index(&[("dash", r#"[{"app":"dashboard"}]"#)]);
        assert!(matched(&idx, &[("app", "Dashboard")]).is_empty());
    }

    #[test]
    fn and_within_a_match() {
        let idx = index(&[("low", r#"[{"app":"dashboard","priority":"low"}]"#)]);
        assert_eq!(
            matched(&idx, &[("app", "dashboard"), ("priority", "low")]),
            [0]
        );
        assert_eq!(
            matched(&idx, &[("priority", "low"), ("app", "dashboard")]),
            [0]
        );
        assert!(matched(&idx, &[("app", "dashboard")]).is_empty());
        assert!(matched(&idx, &[("priority", "low")]).is_empty());
        assert!(matched(&idx, &[("app", "dashboard"), ("priority", "high")]).is_empty());
    }

    #[test]
    fn or_between_matches() {
        let idx = index(&[(
            "dash",
            r#"[{"app":"dashboard"},{"feature":"export_csv","app":"api"}]"#,
        )]);
        assert_eq!(matched(&idx, &[("app", "dashboard")]), [0]);
        assert_eq!(
            matched(&idx, &[("app", "api"), ("feature", "export_csv")]),
            [0]
        );
        assert!(matched(&idx, &[("feature", "export_csv")]).is_empty());
    }

    #[test]
    fn budget_matched_by_several_rules_appears_once() {
        let idx = index(&[(
            "dash",
            r#"[{"app":"dashboard"},{"user":"bob"},{"app":"dashboard","user":"bob"}]"#,
        )]);
        assert_eq!(matched(&idx, &[("app", "dashboard"), ("user", "bob")]), [0]);
    }

    #[test]
    fn several_budgets_come_back_in_declaration_order() {
        let idx = index(&[
            ("by-user", r#"[{"user":"reporting"}]"#),
            ("other", r#"[{"app":"worker"}]"#),
            ("by-app", r#"[{"app":"dashboard"}]"#),
            ("by-both", r#"[{"app":"dashboard","user":"reporting"}]"#),
        ]);
        // Tag order is the reverse of declaration order on purpose.
        assert_eq!(
            matched(&idx, &[("app", "dashboard"), ("user", "reporting")]),
            [0, 2, 3]
        );
    }

    #[test]
    fn empty_rules_match_nothing() {
        let idx = RuleIndex::new(&validate(r#"{"budgets":[]}"#, 64).unwrap());
        assert!(matched(&idx, &[("app", "dashboard")]).is_empty());
    }

    #[test]
    fn works_on_parsed_tags() {
        let idx = index(&[("csv", r#"[{"feature":"export_csv","route":"/reports"}]"#)]);
        let tags = crate::parse_tags("SELECT 1 /*feature='export_csv',route='%2Freports'*/");
        assert_eq!(idx.matching_budgets(&tags).to_vec(), [0]);
    }

    #[test]
    fn tag_ref_hashes_like_owned_key() {
        use std::hash::BuildHasher;
        let s = hashbrown::DefaultHashBuilder::default();
        assert_eq!(
            s.hash_one(TagRef("app", "x")),
            s.hash_one(("app".to_string(), "x".to_string()))
        );
    }
}
