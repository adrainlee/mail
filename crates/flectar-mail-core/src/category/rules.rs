//! Rule model and evaluation. Matching happens in Rust (never in SQL).

use regex::RegexBuilder;
use serde::{Deserialize, Serialize};

/// Compiled regex size cap; protects against pathological patterns.
const REGEX_SIZE_LIMIT: usize = 256 * 1024;
const REGEX_MAX_LEN: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchField {
    FromAddress,
    FromDomain,
    FromName,
    To,
    Subject,
    Body,
}

impl MatchField {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FromAddress => "from_address",
            Self::FromDomain => "from_domain",
            Self::FromName => "from_name",
            Self::To => "to",
            Self::Subject => "subject",
            Self::Body => "body",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "from_address" => Self::FromAddress,
            "from_domain" => Self::FromDomain,
            "from_name" => Self::FromName,
            "to" => Self::To,
            "subject" => Self::Subject,
            "body" => Self::Body,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchOp {
    Equals,
    Contains,
    /// `*` = any run of characters, `?` = exactly one character.
    Wildcard,
    Regex,
    /// Field is non-empty.
    Exists,
}

impl MatchOp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Equals => "equals",
            Self::Contains => "contains",
            Self::Wildcard => "wildcard",
            Self::Regex => "regex",
            Self::Exists => "exists",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "equals" => Self::Equals,
            "contains" => Self::Contains,
            "wildcard" => Self::Wildcard,
            "regex" => Self::Regex,
            "exists" => Self::Exists,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchMode {
    All,
    Any,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Condition {
    pub field: MatchField,
    pub op: MatchOp,
    pub value: String,
    #[serde(default)]
    pub case_sensitive: bool,
    #[serde(default)]
    pub negate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rule {
    #[serde(default)]
    pub id: i64,
    pub category_id: String,
    pub name: String,
    #[serde(default = "default_priority")]
    pub priority: i32,
    #[serde(default = "yes")]
    pub enabled: bool,
    pub mode: MatchMode,
    pub conditions: Vec<Condition>,
    #[serde(default)]
    pub also_star: bool,
    #[serde(default)]
    pub also_mark_read: bool,
}

fn yes() -> bool {
    true
}
fn default_priority() -> i32 {
    100
}

/// What the facts of one message look like to the rule engine.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageFacts {
    pub from_address: String,
    pub from_name: String,
    pub to: Vec<String>,
    pub subject: String,
    pub body: String,
}

impl MessageFacts {
    fn values(&self, field: MatchField) -> Vec<&str> {
        match field {
            MatchField::FromAddress => vec![self.from_address.as_str()],
            MatchField::FromDomain => vec![
                self.from_address
                    .rsplit_once('@')
                    .map(|(_, d)| d)
                    .unwrap_or(""),
            ],
            MatchField::FromName => vec![self.from_name.as_str()],
            MatchField::To => self.to.iter().map(String::as_str).collect(),
            MatchField::Subject => vec![self.subject.as_str()],
            MatchField::Body => vec![self.body.as_str()],
        }
    }
}

/// The only actions a rule may cause. Constructed exclusively by
/// [`RuleMatch::actions`] inside this module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleAction {
    ApplyCategory(String),
    Star,
    MarkRead,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConditionHit {
    pub index: usize,
    pub matched: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleMatch {
    pub rule_id: i64,
    pub category_id: String,
    pub conditions: Vec<ConditionHit>,
    #[serde(skip)]
    also_star: bool,
    #[serde(skip)]
    also_mark_read: bool,
}

impl RuleMatch {
    pub fn actions(&self) -> Vec<RuleAction> {
        let mut out = vec![RuleAction::ApplyCategory(self.category_id.clone())];
        if self.also_star {
            out.push(RuleAction::Star);
        }
        if self.also_mark_read {
            out.push(RuleAction::MarkRead);
        }
        out
    }
}

/// `*` / `?` glob, matched over chars (iterative, no backtracking blow-up).
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Validate a condition (e.g. before saving). Returns a user-facing error.
pub fn validate_condition(c: &Condition) -> Result<(), String> {
    if c.op != MatchOp::Exists && c.value.is_empty() {
        return Err("condition value is empty".into());
    }
    if c.op == MatchOp::Regex {
        build_regex(&c.value, c.case_sensitive)?;
    }
    Ok(())
}

fn build_regex(pattern: &str, case_sensitive: bool) -> Result<regex::Regex, String> {
    if pattern.len() > REGEX_MAX_LEN {
        return Err("regex is too long".into());
    }
    RegexBuilder::new(pattern)
        .case_insensitive(!case_sensitive)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_SIZE_LIMIT)
        .build()
        .map_err(|e| format!("invalid regex: {e}"))
}

fn normalize_host_like(field: MatchField, s: &str) -> String {
    // Addresses and domains compare case-insensitively and IDN-insensitively
    // (punycode `xn--` labels are decoded to Unicode).
    let s = s.trim().trim_end_matches('.').to_lowercase();
    match field {
        MatchField::FromAddress | MatchField::FromDomain | MatchField::To => {
            let (local, domain) = match s.rsplit_once('@') {
                Some((l, d)) => (Some(l.to_string()), d.to_string()),
                None => (None, s.clone()),
            };
            let decoded = url::quirks::domain_to_unicode(&domain);
            let domain = if decoded.is_empty() { domain } else { decoded };
            match local {
                Some(l) => format!("{l}@{domain}"),
                None => domain,
            }
        }
        _ => s,
    }
}

fn condition_matches(c: &Condition, facts: &MessageFacts) -> bool {
    let values = facts.values(c.field);
    let hit = match c.op {
        MatchOp::Exists => values.iter().any(|v| !v.trim().is_empty()),
        MatchOp::Regex => match build_regex(&c.value, c.case_sensitive) {
            Ok(re) => values.iter().any(|v| re.is_match(v)),
            Err(_) => false,
        },
        op => {
            let fold = |s: &str| {
                if c.case_sensitive {
                    s.to_string()
                } else {
                    normalize_host_like(c.field, s)
                }
            };
            let needle = fold(&c.value);
            values.iter().any(|v| {
                let v = fold(v);
                match op {
                    MatchOp::Equals => v == needle,
                    MatchOp::Contains => v.contains(&needle),
                    MatchOp::Wildcard => wildcard_match(&needle, &v),
                    _ => false,
                }
            })
        }
    };
    hit != c.negate
}

/// Evaluate one rule, returning per-condition results (for the test panel).
pub fn evaluate_rule(rule: &Rule, facts: &MessageFacts) -> (bool, Vec<ConditionHit>) {
    let hits: Vec<ConditionHit> = rule
        .conditions
        .iter()
        .enumerate()
        .map(|(index, c)| ConditionHit {
            index,
            matched: condition_matches(c, facts),
        })
        .collect();
    let ok = !hits.is_empty()
        && match rule.mode {
            MatchMode::All => hits.iter().all(|h| h.matched),
            MatchMode::Any => hits.iter().any(|h| h.matched),
        };
    (ok, hits)
}

/// First matching enabled rule by ascending priority (ties: lower id first).
pub fn evaluate(rules: &[Rule], facts: &MessageFacts) -> Option<RuleMatch> {
    let mut ordered: Vec<&Rule> = rules.iter().filter(|r| r.enabled).collect();
    ordered.sort_by_key(|r| (r.priority, r.id));
    ordered.into_iter().find_map(|rule| {
        let (ok, conditions) = evaluate_rule(rule, facts);
        ok.then(|| RuleMatch {
            rule_id: rule.id,
            category_id: rule.category_id.clone(),
            conditions,
            also_star: rule.also_star,
            also_mark_read: rule.also_mark_read,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cond(field: MatchField, op: MatchOp, value: &str) -> Condition {
        Condition {
            field,
            op,
            value: value.into(),
            case_sensitive: false,
            negate: false,
        }
    }

    fn rule(
        id: i64,
        cat: &str,
        priority: i32,
        mode: MatchMode,
        conditions: Vec<Condition>,
    ) -> Rule {
        Rule {
            id,
            category_id: cat.into(),
            name: format!("r{id}"),
            priority,
            enabled: true,
            mode,
            conditions,
            also_star: false,
            also_mark_read: false,
        }
    }

    fn facts() -> MessageFacts {
        MessageFacts {
            from_address: "Robot@Alerts.Hetzner.com".into(),
            from_name: "Hetzner Robot".into(),
            to: vec!["me@example.org".into()],
            subject: "Your Invoice 2026-10".into(),
            body: "Server CPU usage above 95%".into(),
        }
    }

    #[test]
    fn wildcard_semantics() {
        assert!(wildcard_match(
            "*@*.hetzner.com",
            "robot@alerts.hetzner.com"
        ));
        assert!(!wildcard_match("*@*.hetzner.com", "robot@hetzner.com"));
        assert!(wildcard_match("noreply@github.com", "noreply@github.com"));
        assert!(wildcard_match("inv?ice*", "invoice 2026"));
        assert!(!wildcard_match("inv?ice", "invoice 2026"));
        assert!(wildcard_match("*", ""));
        assert!(wildcard_match("a*b*c", "aXXbYYc"));
    }

    #[test]
    fn domain_case_and_idn_are_normalized() {
        let f = facts();
        let r = rule(
            1,
            "vps",
            1,
            MatchMode::All,
            vec![cond(
                MatchField::FromAddress,
                MatchOp::Wildcard,
                "*@*.HETZNER.com",
            )],
        );
        assert!(evaluate(&[r], &f).is_some());
        let idn = MessageFacts {
            from_address: "info@xn--mnchen-3ya.de".into(),
            ..Default::default()
        };
        let r = rule(
            2,
            "x",
            1,
            MatchMode::All,
            vec![cond(MatchField::FromDomain, MatchOp::Equals, "münchen.de")],
        );
        assert!(evaluate(&[r], &idn).is_some());
    }

    #[test]
    fn subject_keyword_regex_negate_and_modes() {
        let f = facts();
        let kw = rule(
            1,
            "bills",
            1,
            MatchMode::All,
            vec![cond(MatchField::Subject, MatchOp::Contains, "invoice")],
        );
        assert_eq!(evaluate(&[kw], &f).unwrap().category_id, "bills");
        let mut neg = cond(MatchField::Body, MatchOp::Contains, "cpu");
        neg.negate = true;
        let all = rule(
            2,
            "a",
            1,
            MatchMode::All,
            vec![
                cond(MatchField::Subject, MatchOp::Contains, "invoice"),
                neg.clone(),
            ],
        );
        assert!(evaluate(&[all], &f).is_none());
        let any = rule(
            3,
            "a",
            1,
            MatchMode::Any,
            vec![
                cond(MatchField::Subject, MatchOp::Contains, "nope"),
                cond(MatchField::Body, MatchOp::Regex, r"usage above \d+%"),
            ],
        );
        let m = evaluate(&[any], &f).unwrap();
        assert_eq!(m.conditions.iter().filter(|h| h.matched).count(), 1);
        let mut cs = cond(MatchField::Subject, MatchOp::Contains, "invoice");
        cs.case_sensitive = true;
        assert!(evaluate(&[rule(4, "a", 1, MatchMode::All, vec![cs])], &f).is_none());
        let empty = rule(5, "a", 1, MatchMode::All, vec![]);
        assert!(
            evaluate(&[empty], &f).is_none(),
            "a rule without conditions never matches"
        );
    }

    #[test]
    fn regex_size_limit_rejects_huge_patterns() {
        let huge = cond(MatchField::Subject, MatchOp::Regex, &"a{1000}".repeat(50));
        assert!(validate_condition(&huge).is_err());
        assert!(validate_condition(&cond(MatchField::Subject, MatchOp::Regex, "(")).is_err());
        assert!(
            validate_condition(&cond(MatchField::Subject, MatchOp::Regex, "invoice|bill")).is_ok()
        );
        // An invalid stored regex never matches rather than panicking.
        assert!(evaluate(&[rule(1, "a", 1, MatchMode::All, vec![huge])], &facts()).is_none());
    }

    #[test]
    fn priority_order_and_disabled_rules() {
        let f = facts();
        let low = rule(
            1,
            "bills",
            50,
            MatchMode::All,
            vec![cond(MatchField::Subject, MatchOp::Contains, "invoice")],
        );
        let high = rule(
            2,
            "vps",
            5,
            MatchMode::All,
            vec![cond(
                MatchField::FromDomain,
                MatchOp::Wildcard,
                "*.hetzner.com",
            )],
        );
        assert_eq!(
            evaluate(&[low.clone(), high.clone()], &f)
                .unwrap()
                .category_id,
            "vps"
        );
        let mut disabled = high;
        disabled.enabled = false;
        assert_eq!(evaluate(&[low, disabled], &f).unwrap().category_id, "bills");
    }

    #[test]
    fn actions_are_limited_to_category_star_and_read() {
        let mut r = rule(
            1,
            "bills",
            1,
            MatchMode::All,
            vec![cond(MatchField::Subject, MatchOp::Contains, "invoice")],
        );
        assert_eq!(
            evaluate(&[r.clone()], &facts()).unwrap().actions(),
            vec![RuleAction::ApplyCategory("bills".into())]
        );
        r.also_star = true;
        r.also_mark_read = true;
        assert_eq!(
            evaluate(&[r], &facts()).unwrap().actions(),
            vec![
                RuleAction::ApplyCategory("bills".into()),
                RuleAction::Star,
                RuleAction::MarkRead
            ]
        );
    }
}
