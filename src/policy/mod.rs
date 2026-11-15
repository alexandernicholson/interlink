use std::sync::Arc;

use arc_swap::ArcSwap;
use dashmap::DashMap;

use crate::common::error::InterlinkError;
use crate::common::identity::{CompiledPattern, SpiffeId};

/// Authorization decision after checking all applicable policies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(&'static str),
}

/// A policy rule that matches a source identity against a target identity.
#[derive(Debug, Clone)]
pub struct PolicyRule {
    /// Pre-compiled SPIFFE ID pattern for the source (caller).
    pub source: CompiledPattern,

    /// Pre-compiled SPIFFE ID pattern for the destination (callee).
    pub destination: CompiledPattern,

    /// Whether to allow or deny matching traffic.
    pub decision: Decision,

    /// Optional description for observability.
    pub description: String,
}

/// The authorization policy engine.
///
/// Evaluates whether a given source identity is allowed to communicate
/// with a given destination identity. Inspired by Istio's AuthorizationPolicy
/// but fully SPIFFE-native.
///
/// Architecture:
/// - `namespace_policies` are keyed by destination namespace for fast lookup
/// - `global_policies` apply to all traffic; matching denies in either scope win
/// - Changes are hot-reloaded via ArcSwap without connection interruption
pub struct PolicyEngine {
    namespace_policies: DashMap<String, Vec<PolicyRule>>,
    global_policies: ArcSwap<Vec<PolicyRule>>,
    default_decision: Decision,
}

impl Default for PolicyEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl PolicyEngine {
    /// Creates a new policy engine with default-deny semantics.
    pub fn new() -> Self {
        Self {
            namespace_policies: DashMap::new(),
            global_policies: ArcSwap::new(Arc::new(Vec::new())),
            default_decision: Decision::Deny("no matching policy"),
        }
    }

    /// Evaluate whether source is allowed to talk to destination.
    ///
    /// Any matching deny outranks every matching allow, regardless of scope.
    /// Within a decision, the first match wins: namespace rules precede global
    /// rules, preserving the first matching deny's reason. If no rule matches,
    /// use the configured default decision (initially deny).
    pub fn evaluate(&self, source: &SpiffeId, destination: &SpiffeId) -> Decision {
        let mut allowed = false;
        // Remember allows, but keep looking for an overriding deny.
        if let Some(rules) = self.namespace_policies.get(&destination.namespace) {
            for rule in rules.iter() {
                if source.matches_compiled(&rule.source)
                    && destination.matches_compiled(&rule.destination)
                {
                    if let Decision::Deny(_) = rule.decision {
                        return rule.decision.clone();
                    }
                    allowed = true;
                }
            }
        }

        // Global denies also override namespace allows.
        for rule in self.global_policies.load().iter() {
            if source.matches_compiled(&rule.source)
                && destination.matches_compiled(&rule.destination)
            {
                if let Decision::Deny(_) = rule.decision {
                    return rule.decision.clone();
                }
                allowed = true;
            }
        }

        if allowed {
            Decision::Allow
        } else {
            self.default_decision.clone()
        }
    }

    /// Add a namespace-scoped policy rule.
    pub fn add_namespace_rule(&self, namespace: &str, rule: PolicyRule) {
        self.namespace_policies
            .entry(namespace.to_string())
            .or_default()
            .push(rule);
    }

    /// Atomically insert or replace the complete rule set for one namespace.
    pub fn set_namespace_policies(&self, namespace: &str, rules: Vec<PolicyRule>) {
        self.namespace_policies.insert(namespace.to_string(), rules);
    }

    /// Replace all global policies atomically.
    pub fn set_global_policies(&self, rules: Vec<PolicyRule>) {
        self.global_policies.store(Arc::new(rules));
    }

    /// Set the default decision (default-deny vs default-allow).
    pub fn set_default_decision(&mut self, decision: Decision) {
        self.default_decision = decision;
    }
}

/// Pre-built policy constructors for common patterns.
pub mod patterns {
    use super::*;

    /// Allow all traffic within the same namespace.
    pub fn allow_same_namespace(
        trust_domain: &str,
        namespace: &str,
    ) -> Result<PolicyRule, InterlinkError> {
        let ns_pat = format!("spiffe://{}/ns/{}/sa/*", trust_domain, namespace);
        let pattern = CompiledPattern::from_uri(&ns_pat)?;
        Ok(PolicyRule {
            source: pattern.clone(),
            destination: pattern,
            decision: Decision::Allow,
            description: format!("Allow all traffic within namespace {}", namespace),
        })
    }

    /// Allow a specific source to talk to a specific destination.
    pub fn allow(
        from: &str,
        to: &str,
        description: &str,
    ) -> Result<PolicyRule, InterlinkError> {
        Ok(PolicyRule {
            source: CompiledPattern::from_uri(from)?,
            destination: CompiledPattern::from_uri(to)?,
            decision: Decision::Allow,
            description: description.to_string(),
        })
    }

    /// Deny all traffic from a specific source.
    pub fn deny_source(source: &str, description: &str) -> Result<PolicyRule, InterlinkError> {
        Ok(PolicyRule {
            source: CompiledPattern::from_uri(source)?,
            destination: CompiledPattern::any(),
            decision: Decision::Deny("blocklisted source"),
            description: description.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_id(ns: &str, sa: &str) -> SpiffeId {
        SpiffeId::new("trust.local", ns, sa).unwrap()
    }

    #[test]
    fn test_default_deny() {
        let engine = PolicyEngine::new();
        let src = make_id("default", "evil");
        let dst = make_id("default", "api");
        assert_eq!(
            engine.evaluate(&src, &dst),
            Decision::Deny("no matching policy")
        );
    }

    #[test]
    fn test_namespace_rule_allow() {
        let engine = PolicyEngine::new();
        engine.add_namespace_rule(
            "default",
            patterns::allow_same_namespace("trust.local", "default").unwrap(),
        );
        let src = make_id("default", "web");
        let dst = make_id("default", "api");
        assert_eq!(engine.evaluate(&src, &dst), Decision::Allow);
    }

    #[test]
    fn test_specific_allow_overrides_default_deny() {
        let engine = PolicyEngine::new();
        engine.add_namespace_rule(
            "default",
            patterns::allow(
                "spiffe://trust.local/ns/default/sa/web",
                "spiffe://trust.local/ns/default/sa/api",
                "web can call api",
            ).unwrap(),
        );
        let src = make_id("default", "web");
        let dst = make_id("default", "api");
        assert_eq!(engine.evaluate(&src, &dst), Decision::Allow);
    }

    #[test]
    fn test_cross_namespace_deny() {
        let engine = PolicyEngine::new();
        engine.add_namespace_rule(
            "billing",
            patterns::allow_same_namespace("trust.local", "billing").unwrap(),
        );
        let src = make_id("default", "web");
        let dst = make_id("billing", "payments");
        // No matching rule for cross-namespace
        assert_eq!(
            engine.evaluate(&src, &dst),
            Decision::Deny("no matching policy")
        );
    }

    #[test]
    fn test_global_policy_overrides_namespace() {
        let engine = PolicyEngine::new();
        engine.add_namespace_rule(
            "default",
            patterns::allow_same_namespace("trust.local", "default").unwrap(),
        );
        engine.set_global_policies(vec![PolicyRule {
            source: CompiledPattern::from_uri("spiffe://trust.local/ns/default/sa/blocked")
                .unwrap(),
            destination: CompiledPattern::from_uri("spiffe://trust.local/ns/*/sa/*").unwrap(),
            decision: Decision::Deny("blocked from all services"),
            description: String::new(),
        }]);
        let src = make_id("default", "blocked");
        let dst = make_id("default", "api");
        assert_eq!(
            engine.evaluate(&src, &dst),
            Decision::Deny("blocked from all services")
        );
    }

    #[test]
    fn test_policy_pattern_wildcard() {
        let engine = PolicyEngine::new();
        engine.add_namespace_rule(
            "default",
            PolicyRule {
                source: CompiledPattern::from_uri("spiffe://trust.local/ns/*/sa/*").unwrap(),
                destination: CompiledPattern::from_uri("spiffe://trust.local/ns/default/sa/api")
                    .unwrap(),
                decision: Decision::Allow,
                description: "any namespace can call api".into(),
            },
        );
        let src = make_id("other", "service");
        let dst = make_id("default", "api");
        assert_eq!(engine.evaluate(&src, &dst), Decision::Allow);
    }

    #[test]
    fn test_default_allow_mode() {
        let mut engine = PolicyEngine::new();
        engine.set_default_decision(Decision::Allow);
        let src = make_id("default", "anything");
        let dst = make_id("other", "anything");
        assert_eq!(engine.evaluate(&src, &dst), Decision::Allow);
    }

    #[test]
    fn test_explicit_deny_rule() {
        let engine = PolicyEngine::new();
        // A matching deny must take precedence over an allow.
        engine.add_namespace_rule(
            "default",
            PolicyRule {
                source: CompiledPattern::from_uri("spiffe://trust.local/ns/default/sa/evil")
                    .unwrap(),
                destination: CompiledPattern::from_uri("spiffe://trust.local/ns/default/sa/*")
                    .unwrap(),
                decision: Decision::Deny("blocked source"),
                description: "deny evil".into(),
            },
        );
        // Allow all traffic within default namespace.
        engine.add_namespace_rule(
            "default",
            patterns::allow_same_namespace("trust.local", "default").unwrap(),
        );

        let evil = make_id("default", "evil");
        let api = make_id("default", "api");
        let good = make_id("default", "good");

        // Deny rule matches first.
        assert_eq!(
            engine.evaluate(&evil, &api),
            Decision::Deny("blocked source")
        );
        // Other traffic is allowed.
        assert_eq!(engine.evaluate(&good, &api), Decision::Allow);
    }

    #[test]
    fn namespace_deny_beats_global_allow() {
        let engine = PolicyEngine::new();
        engine.set_global_policies(vec![
            patterns::allow_same_namespace("trust.local", "default").unwrap(),
        ]);
        engine.add_namespace_rule(
            "default",
            patterns::deny_source("spiffe://trust.local/ns/default/sa/blocked", "blocked").unwrap(),
        );
        assert_eq!(
            engine.evaluate(&make_id("default", "blocked"), &make_id("default", "api")),
            Decision::Deny("blocklisted source")
        );
        assert_eq!(
            engine.evaluate(&make_id("default", "good"), &make_id("default", "api")),
            Decision::Allow
        );
    }

    #[test]
    fn later_deny_beats_allow_and_first_deny_reason_wins() {
        let engine = PolicyEngine::new();
        engine.add_namespace_rule(
            "default",
            patterns::allow_same_namespace("trust.local", "default").unwrap(),
        );
        for reason in ["first deny", "later deny"] {
            let mut rule = patterns::deny_source(
                "spiffe://trust.local/ns/default/sa/blocked", reason,
            ).unwrap();
            rule.decision = Decision::Deny(reason);
            engine.add_namespace_rule("default", rule);
        }
        assert_eq!(
            engine.evaluate(&make_id("default", "blocked"), &make_id("default", "api")),
            Decision::Deny("first deny")
        );
    }

    #[test]
    fn nonmatching_rules_preserve_default_decision() {
        let mut engine = PolicyEngine::new();
        engine.set_global_policies(vec![
            patterns::deny_source("spiffe://trust.local/ns/default/sa/blocked", "blocked").unwrap(),
            patterns::allow_same_namespace("trust.local", "other").unwrap(),
        ]);
        let src = make_id("default", "good");
        let dst = make_id("default", "api");
        assert_eq!(engine.evaluate(&src, &dst), Decision::Deny("no matching policy"));
        engine.set_default_decision(Decision::Allow);
        assert_eq!(engine.evaluate(&src, &dst), Decision::Allow);
    }

    #[test]
    fn policy_pattern_constructors_return_errors_for_invalid_input() {
        let valid = "spiffe://trust.local/ns/default/sa/web";
        assert!(patterns::allow("invalid", valid, "bad source").is_err());
        assert!(patterns::allow(valid, "invalid", "bad destination").is_err());
        assert!(patterns::allow_same_namespace("", "default").is_err());
        assert!(patterns::deny_source("invalid", "bad source").is_err());
        assert_eq!(patterns::allow(valid, valid, "valid").unwrap().decision, Decision::Allow);
        assert_eq!(
            patterns::deny_source(valid, "valid").unwrap().decision,
            Decision::Deny("blocklisted source")
        );
    }

    #[test]
    fn set_namespace_policies_replaces_rules_and_preserves_other_namespaces() {
        let engine = PolicyEngine::new();
        for ns in ["default", "other"] {
            engine.set_namespace_policies(ns, vec![
                patterns::allow_same_namespace("trust.local", ns).unwrap(),
            ]);
        }
        engine.set_namespace_policies("default", vec![
            patterns::deny_source("spiffe://trust.local/ns/default/sa/*", "replacement").unwrap(),
        ]);
        assert_eq!(
            engine.evaluate(&make_id("default", "web"), &make_id("default", "api")),
            Decision::Deny("blocklisted source")
        );
        assert_eq!(
            engine.evaluate(&make_id("other", "web"), &make_id("other", "api")),
            Decision::Allow
        );
        engine.set_namespace_policies("default", vec![]);
        assert_eq!(
            engine.evaluate(&make_id("default", "web"), &make_id("default", "api")),
            Decision::Deny("no matching policy")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn namespace_replacement_never_exposes_partial_rule_sets() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let engine = Arc::new(PolicyEngine::new());
            let mut first = patterns::allow_same_namespace("trust.local", "default").unwrap();
            first.description = "first".into();
            let mut second = first.clone();
            second.description = "second".into();
            engine.set_namespace_policies("default", vec![first.clone(), first.clone()]);
            let gate = Arc::new(tokio::sync::Barrier::new(2));
            let writer_engine = engine.clone();
            let writer_gate = gate.clone();
            let writer = tokio::spawn(async move {
                writer_gate.wait().await;
                for i in 0..1000 {
                    let rule = if i % 2 == 0 { &first } else { &second };
                    writer_engine.set_namespace_policies("default", vec![rule.clone(), rule.clone()]);
                    tokio::task::yield_now().await;
                }
            });
            gate.wait().await;
            for _ in 0..1000 {
                {
                    let rules = engine.namespace_policies.get("default").unwrap();
                    assert_eq!(rules.len(), 2);
                    assert_eq!(rules[0].description, rules[1].description);
                }
                tokio::task::yield_now().await;
            }
            writer.await.unwrap();
        }).await.expect("namespace replacement must not deadlock");
    }
}
