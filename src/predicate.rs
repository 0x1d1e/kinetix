//! Executable Route predicates (FR-12.3, FR-12.4).
//!
//! A predicate is a *deterministic, side-effect-free* expression tree over
//! Kinetix-known request/configuration facts. There is no eval, no code
//! execution, no I/O: only boolean composition and comparisons over facts.
//!
//! Evaluation is **three-valued** (`true` / `false` / `unknown`). A fact
//! Kinetix cannot establish is `unknown`, never guessed. Each evaluation also
//! produces a human-readable explanation for the Route Trace and Dry Run.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::Capabilities;

// ---------------------------------------------------------------------------
// Three-valued logic
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tri {
    True,
    False,
    Unknown,
}

impl Tri {
    pub fn from_bool(b: bool) -> Self {
        if b {
            Tri::True
        } else {
            Tri::False
        }
    }
    pub fn is_true(self) -> bool {
        matches!(self, Tri::True)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Tri::True => "true",
            Tri::False => "false",
            Tri::Unknown => "unknown",
        }
    }
}

/// How an `unknown` predicate result affects target eligibility. The admin
/// must choose; Kinetix never guesses (FR-12.4).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WhenUnknown {
    /// Unknown => target is not eligible (conservative, the default).
    #[default]
    Skip,
    /// Unknown => target remains eligible.
    Allow,
}

// ---------------------------------------------------------------------------
// Facts
// ---------------------------------------------------------------------------

/// A reference to one fact Kinetix can evaluate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FactRef {
    /// `"has_tools"` — a bare fact name with no argument.
    Bare(String),
    /// `{"name": "target_capability", "arg": "vision"}` — a parameterized fact.
    Detailed {
        name: String,
        #[serde(default)]
        arg: Option<String>,
    },
}

impl FactRef {
    pub fn name(&self) -> &str {
        match self {
            FactRef::Bare(n) => n,
            FactRef::Detailed { name, .. } => name,
        }
    }
    pub fn arg(&self) -> Option<&str> {
        match self {
            FactRef::Bare(_) => None,
            FactRef::Detailed { arg, .. } => arg.as_deref(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Lte,
    Gt,
    Gte,
    In,
    NotIn,
    Contains,
}

/// A predicate expression tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Predicate {
    /// `true` / `false` literal.
    Const(bool),
    /// `{"and": [..]}`
    And { and: Vec<Predicate> },
    /// `{"or": [..]}`
    Or { or: Vec<Predicate> },
    /// `{"not": {..}}`
    Not { not: Box<Predicate> },
    /// `{"fact": "has_tools", "op": "eq", "value": true}`
    Cmp {
        fact: FactRef,
        #[serde(default = "default_op")]
        op: CmpOp,
        value: Value,
    },
}

fn default_op() -> CmpOp {
    CmpOp::Eq
}

/// The stored shape of a target predicate: the expression plus the policy for
/// `unknown` results.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TargetPredicate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expr: Option<Predicate>,
    #[serde(default)]
    pub when_unknown: WhenUnknown,
}

impl TargetPredicate {
    pub fn is_empty(&self) -> bool {
        self.expr.is_none()
    }

    /// Parse from the stored JSON. An empty object (`{}`) or `null` means
    /// "always eligible".
    pub fn parse(raw: &str) -> Self {
        if raw.trim().is_empty() || raw.trim() == "{}" || raw.trim() == "null" {
            return TargetPredicate::default();
        }
        // Accept either the envelope `{expr, when_unknown}` or a bare
        // expression tree (treated as `when_unknown: skip`).
        if let Ok(tp) = serde_json::from_str::<TargetPredicate>(raw) {
            if tp.expr.is_some() {
                return tp;
            }
        }
        match serde_json::from_str::<Predicate>(raw) {
            Ok(expr) => TargetPredicate {
                expr: Some(expr),
                when_unknown: WhenUnknown::Skip,
            },
            Err(_) => TargetPredicate::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Evaluation context
// ---------------------------------------------------------------------------

/// Facts about the request itself (not target-specific).
#[derive(Debug, Clone)]
pub struct RequestFacts<'a> {
    pub frontend: &'a str,
    /// The model name the client asked for (alias, Route, or model id).
    pub requested_model: &'a str,
    /// The Route name, when the request resolved to a Route.
    pub requested_route: Option<&'a str>,
    /// The virtual key's free-form tag, when present.
    pub key_tag: Option<&'a str>,
    pub has_tools: bool,
    pub has_images: bool,
    pub has_reasoning: bool,
    pub input_tokens: u64,
}

/// Facts about one candidate target.
#[derive(Debug, Clone)]
pub struct TargetFacts<'a> {
    pub model_id: &'a str,
    pub model_display: &'a str,
    pub provider_id: &'a str,
    pub provider_name: &'a str,
    pub capabilities: &'a Capabilities,
    /// The raw capabilities JSON, used to distinguish "declared false" from
    /// "not configured" (which is `unknown`).
    pub capabilities_raw: &'a Value,
    pub context_window: Option<i64>,
    pub max_output_tokens: Option<i64>,
}

/// Typed facts contributed by plugin RoutingFactProvider capabilities (§6.4).
///
/// Facts are keyed by their fully-qualified name (`plugin.<id>.<name>`). A
/// plugin may contribute several namespaces; a missing or stale fact is simply
/// absent here and therefore evaluates as `unknown`.
#[derive(Debug, Clone)]
pub struct PluginFactEntry {
    pub name: String,
    pub value: Value,
    /// Provenance for the Route Trace (§19): plugin id/version/capability.
    pub source: Value,
}

#[derive(Debug, Clone, Default)]
pub struct PluginFacts {
    entries: Vec<PluginFactEntry>,
    /// Providers whose fact invocation failed this request, with the reason.
    /// Used by the Route Trace so a failed fact is explainable (§6.4, §19).
    pub failures: Vec<(String, String)>,
}

impl PluginFacts {
    pub fn insert(&mut self, name: impl Into<String>, value: Value, source: Value) {
        let name = name.into();
        // A later plugin cannot silently shadow an earlier fact of the same
        // name; first writer wins and the collision is ignored.
        if self.entries.iter().any(|e| e.name == name) {
            return;
        }
        self.entries.push(PluginFactEntry {
            name,
            value,
            source,
        });
    }

    pub fn get(&self, name: &str) -> Option<&Value> {
        self.entries
            .iter()
            .find(|e| e.name == name)
            .map(|e| &e.value)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every fact, for Route Trace rendering.
    pub fn iter_trace(&self) -> impl Iterator<Item = (&str, &Value, &Value)> {
        self.entries
            .iter()
            .map(|e| (e.name.as_str(), &e.value, &e.source))
    }
}

/// The outcome of evaluating a predicate: a three-valued result and the
/// explanation the Route Trace/Dry Run will show.
#[derive(Debug, Clone, Serialize)]
pub struct Eval {
    pub result: Tri,
    pub explanation: String,
}

impl Eval {
    fn known(result: bool, explanation: impl Into<String>) -> Self {
        Eval {
            result: Tri::from_bool(result),
            explanation: explanation.into(),
        }
    }
    fn unknown(explanation: impl Into<String>) -> Self {
        Eval {
            result: Tri::Unknown,
            explanation: explanation.into(),
        }
    }
}

/// Evaluate a fact to a JSON value, or `None` when the fact is unknown.
fn fact_value(
    fact: &FactRef,
    req: &RequestFacts<'_>,
    t: &TargetFacts<'_>,
    plugin: &PluginFacts,
) -> Option<Value> {
    // A plugin-contributed fact is looked up by its fully-qualified name first.
    if fact.name().starts_with("plugin.") {
        return plugin.get(fact.name()).cloned();
    }
    match fact.name() {
        "has_tools" => Some(Value::Bool(req.has_tools)),
        "has_images" => Some(Value::Bool(req.has_images)),
        "has_reasoning" => Some(Value::Bool(req.has_reasoning)),
        "frontend" => Some(Value::String(req.frontend.to_string())),
        "requested_alias" | "requested_model" => {
            Some(Value::String(req.requested_model.to_string()))
        }
        "requested_route" => req.requested_route.map(|r| Value::String(r.to_string())),
        "key_tag" => req.key_tag.map(|t| Value::String(t.to_string())),
        "input_tokens" => Some(Value::Number(req.input_tokens.into())),
        "target_model" | "target_model_id" => Some(Value::String(t.model_id.to_string())),
        "target_provider" | "target_provider_id" => Some(Value::String(t.provider_id.to_string())),
        "target_capability" => {
            let cap = fact.arg()?;
            capability_value(t.capabilities_raw, cap).map(Value::Bool)
        }
        "target_context_window" => t.context_window.map(|v| Value::Number(v.into())),
        "target_max_output_tokens" => t.max_output_tokens.map(|v| Value::Number(v.into())),
        _ => None,
    }
}

/// A known boolean capability value; absent, null, and malformed values remain unknown.
fn capability_value(raw: &Value, name: &str) -> Option<bool> {
    let keys: &[&str] = match name {
        "tool_calling" => &["tool_calling", "tools", "tool_calls"],
        "structured_output" => &["structured_output", "structuredOutput"],
        other => &[other],
    };
    keys.iter()
        .find_map(|key| raw.get(key))
        .and_then(Value::as_bool)
}

/// Evaluate a predicate tree with no plugin facts (native-only).
pub fn eval(pred: &Predicate, req: &RequestFacts<'_>, t: &TargetFacts<'_>) -> Eval {
    eval_with_facts(pred, req, t, &PluginFacts::default())
}

/// Evaluate a predicate tree, including plugin-contributed facts (§6.4).
pub fn eval_with_facts(
    pred: &Predicate,
    req: &RequestFacts<'_>,
    t: &TargetFacts<'_>,
    plugin: &PluginFacts,
) -> Eval {
    match pred {
        Predicate::Const(b) => Eval::known(*b, format!("constant {b}")),
        Predicate::And { and } => {
            let mut parts = Vec::new();
            let mut saw_unknown = false;
            for p in and {
                let e = eval_with_facts(p, req, t, plugin);
                match e.result {
                    Tri::False => {
                        parts.push(format!("[{}]", e.explanation));
                        return Eval::known(
                            false,
                            format!("and: false because {}", parts.join(" ")),
                        );
                    }
                    Tri::Unknown => saw_unknown = true,
                    Tri::True => {}
                }
                parts.push(format!("[{}]", e.explanation));
            }
            if saw_unknown {
                Eval::unknown(format!("and: unknown because {}", parts.join(" ")))
            } else {
                Eval::known(true, format!("and: all true {}", parts.join(" ")))
            }
        }
        Predicate::Or { or } => {
            let mut parts = Vec::new();
            let mut saw_unknown = false;
            for p in or {
                let e = eval_with_facts(p, req, t, plugin);
                match e.result {
                    Tri::True => {
                        parts.push(format!("[{}]", e.explanation));
                        return Eval::known(true, format!("or: true because {}", parts.join(" ")));
                    }
                    Tri::Unknown => saw_unknown = true,
                    Tri::False => {}
                }
                parts.push(format!("[{}]", e.explanation));
            }
            if saw_unknown {
                Eval::unknown(format!("or: unknown because {}", parts.join(" ")))
            } else {
                Eval::known(false, format!("or: all false {}", parts.join(" ")))
            }
        }
        Predicate::Not { not } => {
            let e = eval_with_facts(not, req, t, plugin);
            let result = match e.result {
                Tri::True => Tri::False,
                Tri::False => Tri::True,
                Tri::Unknown => Tri::Unknown,
            };
            Eval {
                result,
                explanation: format!("not [{}]", e.explanation),
            }
        }
        Predicate::Cmp { fact, op, value } => eval_cmp(fact, *op, value, req, t, plugin),
    }
}

fn eval_cmp(
    fact: &FactRef,
    op: CmpOp,
    expected: &Value,
    req: &RequestFacts<'_>,
    t: &TargetFacts<'_>,
    plugin: &PluginFacts,
) -> Eval {
    let Some(actual) = fact_value(fact, req, t, plugin) else {
        return Eval::unknown(format!(
            "fact '{}'{} is unknown",
            fact.name(),
            fact.arg().map(|a| format!("({a})")).unwrap_or_default()
        ));
    };
    let label = format!(
        "{}{} {} {}",
        fact.name(),
        fact.arg().map(|a| format!("({a})")).unwrap_or_default(),
        op_str(op),
        compact(expected)
    );
    let result = match op {
        CmpOp::Eq => json_eq(&actual, expected),
        CmpOp::Ne => !json_eq(&actual, expected),
        CmpOp::Lt | CmpOp::Lte | CmpOp::Gt | CmpOp::Gte => {
            let (Some(a), Some(b)) = (as_f64(&actual), as_f64(expected)) else {
                return Eval::unknown(format!("{label}: not comparable (non-numeric)"));
            };
            match op {
                CmpOp::Lt => a < b,
                CmpOp::Lte => a <= b,
                CmpOp::Gt => a > b,
                CmpOp::Gte => a >= b,
                _ => unreachable!(),
            }
        }
        CmpOp::In | CmpOp::NotIn => {
            let Some(arr) = expected.as_array() else {
                return Eval::unknown(format!("{label}: 'in' value must be an array"));
            };
            let contains = arr.iter().any(|v| json_eq(&actual, v));
            if op == CmpOp::In {
                contains
            } else {
                !contains
            }
        }
        CmpOp::Contains => match (actual, expected) {
            (Value::String(s), Value::String(sub)) => s.contains(sub.as_str()),
            (Value::Array(a), v) => a.iter().any(|x| json_eq(x, v)),
            _ => return Eval::unknown(format!("{label}: 'contains' needs a string or array")),
        },
    };
    Eval::known(result, format!("{label} => {result}"))
}

fn op_str(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Eq => "==",
        CmpOp::Ne => "!=",
        CmpOp::Lt => "<",
        CmpOp::Lte => "<=",
        CmpOp::Gt => ">",
        CmpOp::Gte => ">=",
        CmpOp::In => "in",
        CmpOp::NotIn => "not in",
        CmpOp::Contains => "contains",
    }
}

fn json_eq(a: &Value, b: &Value) -> bool {
    if a == b {
        return true;
    }
    // Numeric cross-type equality (1 == 1.0).
    if let (Some(x), Some(y)) = (as_f64(a), as_f64(b)) {
        return (x - y).abs() < f64::EPSILON;
    }
    false
}

fn as_f64(v: &Value) -> Option<f64> {
    v.as_f64()
}

fn compact(v: &Value) -> String {
    let s = v.to_string();
    if s.len() > 60 {
        format!("{}…", &s[..60])
    } else {
        s
    }
}

/// The full eligibility decision for one target, ready for the Route Trace.
#[derive(Debug, Clone, Serialize)]
pub struct Eligibility {
    pub eligible: bool,
    pub result: Tri,
    pub explanation: String,
}

/// Decide eligibility from a target predicate (native facts only).
pub fn eligibility(
    pred: &TargetPredicate,
    req: &RequestFacts<'_>,
    t: &TargetFacts<'_>,
) -> Eligibility {
    eligibility_with_facts(pred, req, t, &PluginFacts::default())
}

/// Decide eligibility from a target predicate, including plugin facts (§6.4).
pub fn eligibility_with_facts(
    pred: &TargetPredicate,
    req: &RequestFacts<'_>,
    t: &TargetFacts<'_>,
    plugin: &PluginFacts,
) -> Eligibility {
    let Some(expr) = &pred.expr else {
        return Eligibility {
            eligible: true,
            result: Tri::True,
            explanation: "no predicate configured".to_string(),
        };
    };
    let e = eval_with_facts(expr, req, t, plugin);
    let eligible = match e.result {
        Tri::True => true,
        Tri::False => false,
        Tri::Unknown => pred.when_unknown == WhenUnknown::Allow,
    };
    Eligibility {
        eligible,
        result: e.result,
        explanation: format!(
            "{} (when_unknown={})",
            e.explanation,
            match pred.when_unknown {
                WhenUnknown::Skip => "skip",
                WhenUnknown::Allow => "allow",
            }
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req<'a>(tools: bool) -> RequestFacts<'a> {
        RequestFacts {
            frontend: "openai",
            requested_model: "coder",
            requested_route: None,
            key_tag: Some("dev"),
            has_tools: tools,
            has_images: false,
            has_reasoning: false,
            input_tokens: 100,
        }
    }

    fn target<'a>(caps: &'a Capabilities, raw: &'a Value) -> TargetFacts<'a> {
        TargetFacts {
            model_id: "m1",
            model_display: "Model One",
            provider_id: "p1",
            provider_name: "Provider One",
            capabilities: caps,
            capabilities_raw: raw,
            context_window: Some(128_000),
            max_output_tokens: None,
        }
    }

    #[test]
    fn three_valued_and() {
        let p: Predicate = serde_json::from_value(json!({
            "and": [
                {"fact": "has_tools", "op": "eq", "value": true},
                {"fact": "has_images", "op": "eq", "value": false}
            ]
        }))
        .unwrap();
        let caps = Capabilities::default();
        let raw = json!({});
        assert!(eval(&p, &req(true), &target(&caps, &raw)).result.is_true());
        assert_eq!(
            eval(&p, &req(false), &target(&caps, &raw)).result,
            Tri::False
        );
    }

    #[test]
    fn unknown_capability_is_unknown_then_skipped() {
        let pred = TargetPredicate {
            expr: Some(
                serde_json::from_value(json!({
                    "fact": {"name": "target_capability", "arg": "vision"},
                    "op": "eq",
                    "value": true
                }))
                .unwrap(),
            ),
            when_unknown: WhenUnknown::Skip,
        };
        let caps = Capabilities::default();
        let raw = json!({}); // vision not declared
        let e = eligibility(&pred, &req(true), &target(&caps, &raw));
        assert_eq!(e.result, Tri::Unknown);
        assert!(!e.eligible);
    }

    #[test]
    fn declared_capability_is_known() {
        let pred = TargetPredicate {
            expr: Some(
                serde_json::from_value(json!({
                    "fact": {"name": "target_capability", "arg": "vision"},
                    "op": "eq",
                    "value": true
                }))
                .unwrap(),
            ),
            when_unknown: WhenUnknown::Skip,
        };
        let caps = Capabilities {
            vision: true,
            ..Default::default()
        };
        let raw = json!({"vision": true});
        let e = eligibility(&pred, &req(true), &target(&caps, &raw));
        assert!(e.eligible);
    }

    #[test]
    fn explicit_null_capability_does_not_fall_through_to_an_alias() {
        assert_eq!(
            capability_value(
                &json!({"tool_calling": null, "tools": true}),
                "tool_calling"
            ),
            None
        );
        assert_eq!(
            capability_value(&json!({"tools": true}), "tool_calling"),
            Some(true)
        );
    }

    #[test]
    fn numeric_comparison() {
        let p: Predicate = serde_json::from_value(json!({
            "fact": "input_tokens", "op": "lt", "value": 200
        }))
        .unwrap();
        let caps = Capabilities::default();
        let raw = json!({});
        assert!(eval(&p, &req(true), &target(&caps, &raw)).result.is_true());
    }

    #[test]
    fn plugin_fact_is_evaluated_by_qualified_name() {
        let caps = Capabilities::default();
        let raw = json!({});
        // {"fact": "plugin.dev.example.foo.region", "op": "eq", "value": "eu"}
        let p: Predicate = serde_json::from_value(json!({
            "fact": "plugin.dev.example.foo.region",
            "op": "eq",
            "value": "eu"
        }))
        .unwrap();
        // With no plugin facts the fact is unknown, so it is skipped by default.
        assert_eq!(
            eval(&p, &req(true), &target(&caps, &raw)).result,
            Tri::Unknown
        );
        // With the fact present, it evaluates.
        let mut facts = PluginFacts::default();
        facts.insert(
            "plugin.dev.example.foo.region",
            json!("eu"),
            json!({"plugin": "dev.example.foo", "capability": "routing-facts"}),
        );
        let e = eval_with_facts(&p, &req(true), &target(&caps, &raw), &facts);
        assert!(e.result.is_true(), "{}", e.explanation);

        // A stale fact is absent, so it stays unknown.
        let mut stale = PluginFacts::default();
        stale.insert("plugin.other.pinned", json!(true), json!({}));
        let p2: Predicate = serde_json::from_value(json!({
            "fact": "plugin.dev.example.foo.region",
            "op": "eq",
            "value": "eu"
        }))
        .unwrap();
        assert!(
            eval_with_facts(&p2, &req(true), &target(&caps, &raw), &stale).result == Tri::Unknown
        );
    }

    #[test]
    fn a_plugin_fact_cannot_shadow_an_earlier_one() {
        let mut facts = PluginFacts::default();
        facts.insert("plugin.a.x", json!(1), json!({"plugin": "a"}));
        facts.insert("plugin.a.x", json!(2), json!({"plugin": "b"}));
        assert_eq!(facts.get("plugin.a.x"), Some(&json!(1)));
        assert_eq!(facts.iter_trace().count(), 1);
    }
}
