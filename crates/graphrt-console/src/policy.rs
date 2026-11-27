//! The role each route of `graphrt serve` needs from a signed in person, and
//! what is written to the audit log when someone uses it.
//!
//! Routes are matched by method and path; `*` stands for one path segment
//! and `**` for the rest of the path. A route the table does not name needs
//! an owner: a new route is closed until it is listed here.
//!
//! Requests with a bearer token keep their own rules (the API key may do
//! everything, a device token what its scopes allow, an end user token
//! what end users may), and the actions marked `bearer` are audited for them too.

use axum::http::Method;

use crate::roles::Role;

/// What a route needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rule {
    /// The least role that may use it; `None` for a public route (a page,
    /// a sign in), which a session does not change.
    pub role: Option<Role>,
    /// The audit action, for routes that change something.
    pub action: Option<&'static str>,
    /// Whether requests with a bearer token are audited too (owner actions,
    /// approvals and devices; not run starts, which the run store records).
    pub bearer: bool,
    /// Whether a signed in browser may use it. Webhook deliveries are for the
    /// systems that send them: a hook may map request headers into run
    /// inputs, and the key a session goes on with must not land there.
    pub sessions: bool,
}

const PUBLIC: Rule = Rule { role: None, action: None, bearer: false, sessions: true };

const fn read(role: Role) -> Rule {
    Rule { role: Some(role), action: None, bearer: false, sessions: true }
}

const fn change(role: Role, action: &'static str, bearer: bool) -> Rule {
    Rule { role: Some(role), action: Some(action), bearer, sessions: true }
}

const fn machines_only(action: &'static str) -> Rule {
    Rule { role: Some(Role::Owner), action: Some(action), bearer: false, sessions: false }
}

use Role::{Operator, Owner, Viewer};

/// `(method, path pattern, rule)`, first match wins. `ANY` matches every method.
const ROUTES: &[(&str, &str, Rule)] = &[
    // Pages and what they read before anyone signs in.
    ("GET", "/console", PUBLIC),
    ("GET", "/console/**", PUBLIC),
    ("GET", "/v1/console/session", PUBLIC),
    ("GET", "/monitor", PUBLIC),
    ("GET", "/monitor/**", PUBLIC),
    ("GET", "/v1/monitor/config", PUBLIC),
    ("GET", "/chat", PUBLIC),
    ("ANY", "/chat/**", PUBLIC),
    ("ANY", "/v1/chat/**", PUBLIC),
    // Workflows and runs.
    ("GET", "/v1/workflows", read(Viewer)),
    ("POST", "/v1/workflows/*/runs", change(Operator, "run.start", false)),
    ("GET", "/v1/runs", read(Viewer)),
    ("GET", "/v1/runs/*", read(Viewer)),
    ("DELETE", "/v1/runs/*", change(Operator, "run.delete", true)),
    ("GET", "/v1/runs/*/events", read(Viewer)),
    ("GET", "/v1/runs/*/scores", read(Viewer)),
    ("POST", "/v1/runs/*/abort", change(Operator, "run.abort", true)),
    ("POST", "/v1/runs/*/resume", change(Operator, "run.resume", true)),
    ("POST", "/v1/runs/*/feedback", change(Operator, "run.feedback", false)),
    ("GET", "/v1/nodes", read(Viewer)),
    ("POST", "/v1/validate", read(Viewer)),
    ("GET", "/v1/hooks", read(Viewer)),
    ("POST", "/v1/hooks/*", machines_only("hook.trigger")),
    // Messaging channels: deliveries of Telegram and WhatsApp, which sign them their own way.
    ("ANY", "/v1/channels/**", machines_only("channel.delivery")),
    // Approvals, devices, push.
    ("GET", "/v1/approvals", read(Viewer)),
    ("POST", "/v1/approvals/*", change(Operator, "approval.resolve", true)),
    ("GET", "/v1/devices", read(Operator)),
    ("POST", "/v1/devices", change(Operator, "device.pair", true)),
    ("DELETE", "/v1/devices/me", change(Owner, "device.unpair", true)),
    ("DELETE", "/v1/devices/*", change(Operator, "device.revoke", true)),
    ("GET", "/v1/push", read(Viewer)),
    ("POST", "/v1/push/subscriptions", change(Viewer, "push.subscribe", false)),
    ("DELETE", "/v1/push/subscriptions", change(Viewer, "push.unsubscribe", false)),
    ("POST", "/v1/push/test", change(Viewer, "push.test", false)),
    // Knowledge, skills, lessons: read by viewers, changed by owners.
    ("POST", "/v1/knowledge/retrieve", read(Viewer)),
    ("GET", "/v1/knowledge/**", read(Viewer)),
    ("ANY", "/v1/knowledge/**", change(Owner, "knowledge.change", true)),
    ("GET", "/v1/skills", read(Viewer)),
    ("GET", "/v1/skills/**", read(Viewer)),
    ("ANY", "/v1/skills", change(Owner, "skill.change", true)),
    ("ANY", "/v1/skills/**", change(Owner, "skill.change", true)),
    ("GET", "/v1/learning/**", read(Viewer)),
    ("ANY", "/v1/learning/**", change(Owner, "learning.change", true)),
    // End users' conversations and the agents' files: owners.
    ("GET", "/v1/conversations", read(Owner)),
    ("GET", "/v1/conversations/**", read(Owner)),
    ("ANY", "/v1/conversations", change(Owner, "conversation.change", true)),
    ("ANY", "/v1/conversations/**", change(Owner, "conversation.change", true)),
    ("GET", "/v1/files", read(Owner)),
    ("GET", "/v1/files/**", read(Owner)),
    ("ANY", "/v1/files/**", change(Owner, "file.change", true)),
    // The console's own API: the handlers write what they did to the audit log.
    ("POST", "/v1/console/sign-out", read(Viewer)),
    ("GET", "/v1/console/workflows", read(Viewer)),
    ("GET", "/v1/console/workflows/*/versions/*/file", read(Owner)),
    ("GET", "/v1/console/workflows/**", read(Viewer)),
    ("POST", "/v1/console/workflows/**", read(Owner)),
    ("GET", "/v1/console/secrets", read(Operator)),
    ("ANY", "/v1/console/secrets/*", read(Owner)),
    ("GET", "/v1/console/members", read(Owner)),
    ("ANY", "/v1/console/members/*", read(Owner)),
    ("GET", "/v1/console/sessions", read(Owner)),
    ("DELETE", "/v1/console/sessions/*", read(Owner)),
    ("GET", "/v1/console/audit", read(Owner)),
    ("GET", "/v1/console/audit/**", read(Owner)),
    ("POST", "/v1/console/apply", read(Owner)),
];

/// The rule of a request.
pub fn rule(method: &Method, path: &str) -> Rule {
    let path = path.trim_end_matches('/');
    let path = if path.is_empty() { "/" } else { path };
    for (route_method, pattern, rule) in ROUTES {
        if method_matches(route_method, method) && matches(pattern, path) {
            return *rule;
        }
    }
    let changes = !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS);
    Rule { role: Some(Role::Owner), action: changes.then_some("api.change"), bearer: changes, sessions: true }
}

/// `HEAD` asks what `GET` would answer.
fn method_matches(route_method: &str, method: &Method) -> bool {
    route_method == "ANY" || route_method == method.as_str() || (route_method == "GET" && *method == Method::HEAD)
}

/// The path segments a pattern's `*` and `**` matched: the audit target.
pub fn target(method: &Method, path: &str) -> Option<String> {
    let path = path.trim_end_matches('/');
    for (route_method, pattern, _) in ROUTES {
        if method_matches(route_method, method) && matches(pattern, path) {
            let mut parts = Vec::new();
            let mut actual = path.split('/');
            for expected in pattern.split('/') {
                match expected {
                    "**" => {
                        parts.extend(actual.by_ref());
                        break;
                    }
                    "*" => parts.extend(actual.next()),
                    _ => {
                        actual.next();
                    }
                }
            }
            return (!parts.is_empty()).then(|| parts.join("/"));
        }
    }
    None
}

fn matches(pattern: &str, path: &str) -> bool {
    let mut actual = path.split('/');
    for expected in pattern.split('/') {
        if expected == "**" {
            return actual.next().is_some_and(|segment| !segment.is_empty());
        }
        match actual.next() {
            Some(segment) if expected == "*" && !segment.is_empty() => {}
            Some(segment) if segment == expected => {}
            _ => return false,
        }
    }
    actual.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(method: Method, path: &str) -> Option<Role> {
        rule(&method, path).role
    }

    #[test]
    fn roles_follow_the_table() {
        assert_eq!(role(Method::GET, "/v1/runs"), Some(Viewer));
        assert_eq!(role(Method::GET, "/v1/runs/r1/events"), Some(Viewer));
        assert_eq!(role(Method::POST, "/v1/runs/r1/abort"), Some(Operator));
        assert_eq!(role(Method::POST, "/v1/workflows/support/runs"), Some(Operator));
        assert_eq!(role(Method::POST, "/v1/approvals/r1"), Some(Operator));
        assert_eq!(role(Method::DELETE, "/v1/devices/d1"), Some(Operator));
        assert_eq!(role(Method::DELETE, "/v1/devices/me"), Some(Owner));
        assert_eq!(role(Method::PATCH, "/v1/skills/web"), Some(Owner));
        assert_eq!(role(Method::GET, "/v1/skills/web"), Some(Viewer));
        assert_eq!(role(Method::PUT, "/v1/console/secrets/OPENAI_API_KEY"), Some(Owner));
        assert_eq!(role(Method::GET, "/v1/console/secrets"), Some(Operator));
        assert_eq!(role(Method::GET, "/v1/console/workflows/support/versions/3/file"), Some(Owner));
        assert_eq!(role(Method::GET, "/v1/console/workflows/support/versions"), Some(Viewer));
    }

    #[test]
    fn pages_are_public_and_unknown_routes_need_an_owner() {
        assert_eq!(role(Method::GET, "/monitor/"), None);
        assert_eq!(role(Method::GET, "/console/callback"), None);
        assert_eq!(role(Method::GET, "/v1/console/session"), None);
        assert_eq!(role(Method::POST, "/v1/chat/guest"), None);
        let unknown = rule(&Method::POST, "/v1/something-new");
        assert_eq!((unknown.role, unknown.action, unknown.bearer), (Some(Owner), Some("api.change"), true));
        assert_eq!(role(Method::GET, "/v1/something-new"), Some(Owner));
        // A pattern never matches more segments than it has.
        assert_eq!(role(Method::GET, "/v1/runs/r1/events/extra"), Some(Owner));
        assert_eq!(role(Method::GET, "/v1/runs//events"), Some(Owner));
    }

    #[test]
    fn targets_are_the_matched_segments() {
        assert_eq!(target(&Method::POST, "/v1/approvals/r-1"), Some("r-1".into()));
        assert_eq!(target(&Method::PATCH, "/v1/skills/web/x"), Some("web/x".into()));
        assert_eq!(target(&Method::GET, "/v1/runs"), None);
    }

    #[test]
    fn bearer_requests_audit_operator_and_owner_actions_only() {
        assert!(rule(&Method::POST, "/v1/approvals/r1").bearer);
        assert!(rule(&Method::POST, "/v1/devices").bearer);
        assert!(!rule(&Method::POST, "/v1/workflows/w/runs").bearer);
        assert!(!rule(&Method::POST, "/v1/hooks/h").bearer);
    }

    #[test]
    fn webhooks_are_not_for_browsers_and_head_reads() {
        assert!(!rule(&Method::POST, "/v1/hooks/h").sessions);
        assert!(!rule(&Method::POST, "/v1/channels/support/webhook").sessions);
        assert!(rule(&Method::POST, "/v1/approvals/r").sessions);
        assert_eq!(role(Method::HEAD, "/v1/runs"), Some(Viewer));
    }
}
