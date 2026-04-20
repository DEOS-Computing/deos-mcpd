use anyhow::{Context, Result};
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Allow,
    Deny,
    RequireApproval,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArgMatch {
    Equals(Value),
    Regex(String),
    StartsWith(String),
    NotStartsWith(String),
    Contains(String),
}

impl ArgMatch {
    pub fn matches(&self, v: &Value) -> bool {
        match self {
            ArgMatch::Equals(expected) => v == expected,
            ArgMatch::Regex(pat) => match v.as_str() {
                Some(s) => Regex::new(pat).map(|r| r.is_match(s)).unwrap_or(false),
                None => false,
            },
            ArgMatch::StartsWith(pfx) => v.as_str().map(|s| s.starts_with(pfx)).unwrap_or(false),
            ArgMatch::NotStartsWith(pfx) => v.as_str().map(|s| !s.starts_with(pfx)).unwrap_or(true),
            ArgMatch::Contains(needle) => {
                v.as_str().map(|s| s.contains(needle)).unwrap_or(false)
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Rule {
    pub id: String,
    #[serde(default)]
    pub description: Option<String>,
    pub tool: String,
    #[serde(default)]
    pub tool_regex: bool,
    #[serde(default)]
    pub args: std::collections::BTreeMap<String, ArgMatch>,
    pub action: Action,
    /// Timeout for require_approval verdicts (seconds). Default 120.
    #[serde(default)]
    pub approval_timeout_s: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Policy {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default = "default_default_action")]
    pub default: Action,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

fn default_version() -> u32 {
    1
}
fn default_default_action() -> Action {
    Action::Allow
}

impl Policy {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading policy file {:?}", path))?;
        let p: Policy = serde_yaml::from_str(&text).context("parsing policy YAML")?;
        Ok(p)
    }

    pub fn open_default() -> Self {
        Policy {
            version: 1,
            default: Action::Allow,
            rules: vec![],
        }
    }

    /// Evaluate against a tool_name + arguments value. Returns the matching
    /// rule (if any) and the resolved action.
    pub fn evaluate<'a>(&'a self, tool_name: &str, arguments: &Value) -> Verdict<'a> {
        for rule in &self.rules {
            if !tool_matches(&rule.tool, rule.tool_regex, tool_name) {
                continue;
            }
            let args_ok = rule.args.iter().all(|(field, matcher)| {
                let slot = arguments.get(field).unwrap_or(&Value::Null);
                matcher.matches(slot)
            });
            if !args_ok {
                continue;
            }
            return Verdict {
                rule: Some(rule),
                action: rule.action,
            };
        }
        Verdict {
            rule: None,
            action: self.default,
        }
    }
}

fn tool_matches(pat: &str, is_regex: bool, tool: &str) -> bool {
    if is_regex {
        Regex::new(pat).map(|r| r.is_match(tool)).unwrap_or(false)
    } else {
        pat == tool
    }
}

#[derive(Debug)]
pub struct Verdict<'a> {
    pub rule: Option<&'a Rule>,
    pub action: Action,
}

impl<'a> Verdict<'a> {
    pub fn rule_id(&self) -> &str {
        self.rule.map(|r| r.id.as_str()).unwrap_or("<default>")
    }
    pub fn approval_timeout(&self) -> std::time::Duration {
        let s = self.rule.and_then(|r| r.approval_timeout_s).unwrap_or(120);
        std::time::Duration::from_secs(s)
    }
}
