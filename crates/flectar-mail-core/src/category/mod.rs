//! Siftmail M2: user-managed categories and deterministic rules.
//!
//! Rules are the only automatic path that may change a thread, and only via
//! [`RuleAction`] (apply category, optionally star / mark read when the user
//! ticked it). AI and embedding output never constructs these; the model's
//! opinion is stored separately as a `crate::suggest::Suggestion`.

pub mod repo;
pub mod rules;
pub mod suggest_llm;

pub use rules::{
    Condition, ConditionHit, MatchField, MatchMode, MatchOp, MessageFacts, Rule, RuleAction,
    RuleMatch, evaluate,
};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Category {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default = "yes")]
    pub show_as_tab: bool,
    #[serde(default = "default_priority")]
    pub priority: i32,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Local label backing the category tab, when one is linked.
    #[serde(default)]
    pub label_id: Option<i64>,
}

fn yes() -> bool {
    true
}
fn default_priority() -> i32 {
    100
}
