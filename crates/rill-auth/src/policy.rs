//! `policy.toml` parsing and the authorize() decision (security.md §6).
//! First matching rule wins; no match → deny; strict parsing throughout.

use crate::pattern::Pattern;
use crate::{AuthError, Identity};

/// What a request wants to do with a path. A closed pair, deliberately:
/// reading and mutating are the two things the protocol can express, and a
/// policy language that grows a verb per feature stops being auditable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// GET, HEAD, GET_IF.
    Read,
    /// ACTION.
    Act,
}

#[derive(Debug)]
struct Rule {
    pattern: Pattern,
    allow: Vec<String>,
    /// Who may *act* here. `None` means the rule does not distinguish, and
    /// `allow` answers both verbs — which is what every policy written
    /// before this existed means, and it must keep meaning it.
    allow_actions: Option<Vec<String>>,
}

#[derive(Debug)]
pub struct Policy {
    rules: Vec<Rule>,
}

impl Policy {
    pub fn parse(text: &str) -> Result<Policy, AuthError> {
        let table: toml::Table = text
            .parse()
            .map_err(|e| AuthError::new(format!("policy.toml: {e}")))?;

        for key in table.keys() {
            if key != "default_access" && key != "rule" {
                return Err(AuthError::new(format!("policy.toml: unknown key {key:?}")));
            }
        }
        match table.get("default_access").and_then(|v| v.as_str()) {
            Some("deny") => {}
            Some(other) => {
                return Err(AuthError::new(format!(
                    "policy.toml: default_access must be \"deny\", got {other:?}"
                )));
            }
            None => {
                return Err(AuthError::new(
                    "policy.toml: missing `default_access = \"deny\"`",
                ));
            }
        }

        let mut rules = Vec::new();
        if let Some(value) = table.get("rule") {
            let list = value
                .as_array()
                .ok_or_else(|| AuthError::new("policy.toml: `rule` must be [[rule]] tables"))?;
            for (i, entry) in list.iter().enumerate() {
                let rule = entry.as_table().ok_or_else(|| {
                    AuthError::new(format!("policy.toml: rule {} is not a table", i + 1))
                })?;
                for key in rule.keys() {
                    if key != "path" && key != "allow" && key != "allow_actions" {
                        return Err(AuthError::new(format!(
                            "policy.toml: rule {}: unknown key {key:?}",
                            i + 1
                        )));
                    }
                }
                let path = rule.get("path").and_then(|v| v.as_str()).ok_or_else(|| {
                    AuthError::new(format!("policy.toml: rule {}: missing path", i + 1))
                })?;
                let names = |key: &str| -> Result<Option<Vec<String>>, AuthError> {
                    let Some(value) = rule.get(key) else { return Ok(None) };
                    let list = value.as_array().ok_or_else(|| {
                        AuthError::new(format!("policy.toml: rule {}: {key} must be a list", i + 1))
                    })?;
                    let mut out = Vec::new();
                    for item in list {
                        let name = item.as_str().ok_or_else(|| {
                            AuthError::new(format!(
                                "policy.toml: rule {}: {key} entries must be strings",
                                i + 1
                            ))
                        })?;
                        out.push(name.to_string());
                    }
                    Ok(Some(out))
                };
                let allow = names("allow")?.ok_or_else(|| {
                    AuthError::new(format!("policy.toml: rule {}: missing allow list", i + 1))
                })?;
                if allow.is_empty() {
                    return Err(AuthError::new(format!(
                        "policy.toml: rule {}: empty allow list (omit the rule; default is deny)",
                        i + 1
                    )));
                }
                // An empty `allow_actions` is meaningful where an empty
                // `allow` is not: "readable here, and nobody may act."
                let allow_actions = names("allow_actions")?;
                rules.push(Rule { pattern: Pattern::parse(path)?, allow, allow_actions });
            }
        }
        Ok(Policy { rules })
    }

    /// May this identity *read* here? First matching rule decides; no match
    /// denies (security.md §6).
    pub fn authorize(&self, identity: &Identity, path: &str) -> bool {
        self.authorize_access(identity, Access::Read, path)
    }

    /// May this identity do `access` here?
    ///
    /// The rule that matches the path answers for both verbs — there is no
    /// falling through to a later rule when one grants reads and not
    /// actions, because "the first matching rule decides" is the property
    /// that makes a policy file readable top to bottom.
    pub fn authorize_access(&self, identity: &Identity, access: Access, path: &str) -> bool {
        for rule in &self.rules {
            if rule.pattern.matches(path) {
                let list = match access {
                    Access::Read => &rule.allow,
                    // A rule that says nothing about actions is a rule from
                    // before actions were distinguished: `allow` answers.
                    Access::Act => rule.allow_actions.as_ref().unwrap_or(&rule.allow),
                };
                return list.iter().any(|entry| match identity {
                    // "anonymous" grants everyone — public means public.
                    _ if entry == "anonymous" => true,
                    Identity::Device(name) => entry == name,
                    Identity::Anonymous => false,
                });
            }
        }
        false
    }

    /// Startup lint: rules that can never fire because an earlier rule's
    /// pattern covers theirs (security.md §6).
    pub fn lint(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        for (i, rule) in self.rules.iter().enumerate() {
            for earlier in &self.rules[..i] {
                if earlier.pattern.covers(&rule.pattern) {
                    warnings.push(format!(
                        "rule {} ({:?}) is unreachable: shadowed by earlier rule {:?}",
                        i + 1,
                        rule.pattern.source(),
                        earlier.pattern.source()
                    ));
                    break;
                }
            }
        }
        warnings
    }
}

#[cfg(test)]
mod tests {
    use super::{Access, Policy};
    use crate::Identity;

    const EXAMPLE: &str = r#"
default_access = "deny"

[[rule]]
path = "/public/**"
allow = ["anonymous"]

[[rule]]
path = "/private/**"
allow = ["desktop", "laptop"]
"#;

    #[test]
    fn plan_verification_matrix() {
        let policy = Policy::parse(EXAMPLE).unwrap();
        let anon = Identity::Anonymous;
        let desktop = Identity::Device("desktop".into());
        let friend = Identity::Device("friend-laptop".into());

        assert!(policy.authorize(&anon, "/public/index"));
        assert!(!policy.authorize(&anon, "/private/notes"));
        assert!(policy.authorize(&desktop, "/private/notes"));
        assert!(!policy.authorize(&friend, "/private/notes"));
        // Enrolling never reduces access: devices still see public.
        assert!(policy.authorize(&desktop, "/public/index"));
        assert!(policy.authorize(&friend, "/public/index"));
        // Unmatched paths deny for everyone.
        assert!(!policy.authorize(&desktop, "/other/thing"));
    }

    /// A policy written before actions were distinguished keeps meaning
    /// exactly what it meant: `allow` answers both verbs. This is the
    /// compatibility promise, and it is the whole reason `allow_actions` is
    /// an option rather than a second required list.
    #[test]
    fn a_rule_without_allow_actions_answers_both_verbs() {
        let policy = Policy::parse(EXAMPLE).unwrap();
        let desktop = Identity::Device("desktop".into());
        for access in [Access::Read, Access::Act] {
            assert!(policy.authorize_access(&desktop, access, "/private/notes"));
            assert!(policy.authorize_access(&Identity::Anonymous, access, "/public/index"));
            assert!(!policy.authorize_access(&Identity::Anonymous, access, "/private/notes"));
        }
    }

    /// The point of the verb: read-only identities. A laptop that may see
    /// the notes but not change them, and a rule where nobody may act at
    /// all — which an empty list says and an omitted one cannot.
    #[test]
    fn actions_can_be_a_subset_of_readers() {
        let text = r#"
default_access = "deny"

[[rule]]
path = "/notes/**"
allow = ["desktop", "laptop"]
allow_actions = ["desktop"]

[[rule]]
path = "/archive/**"
allow = ["desktop", "laptop"]
allow_actions = []
"#;
        let policy = Policy::parse(text).unwrap();
        let desktop = Identity::Device("desktop".into());
        let laptop = Identity::Device("laptop".into());

        assert!(policy.authorize_access(&desktop, Access::Read, "/notes/a"));
        assert!(policy.authorize_access(&laptop, Access::Read, "/notes/a"));
        assert!(policy.authorize_access(&desktop, Access::Act, "/notes/actions/save/a"));
        assert!(
            !policy.authorize_access(&laptop, Access::Act, "/notes/actions/save/a"),
            "the laptop reads the notes; it does not write them"
        );

        assert!(policy.authorize_access(&laptop, Access::Read, "/archive/old"));
        for who in [&desktop, &laptop] {
            assert!(
                !policy.authorize_access(who, Access::Act, "/archive/old"),
                "an empty allow_actions is a real statement: nobody acts here"
            );
        }

        // `authorize` is still the read verb, so every existing caller —
        // including the file explorer's own listing filter — is unchanged.
        assert!(policy.authorize(&laptop, "/notes/a"));
    }

    #[test]
    fn strict_parsing() {
        assert!(Policy::parse("").is_err()); // missing default_access
        assert!(Policy::parse("default_access = \"allow\"").is_err());
        assert!(Policy::parse("default_access = \"deny\"\nextra = 1").is_err());
        let bad_rule = "default_access = \"deny\"\n[[rule]]\npath = \"/a\"\nallow = []\n";
        assert!(Policy::parse(bad_rule).is_err()); // empty allow
        let bad_pattern = "default_access = \"deny\"\n[[rule]]\npath = \"/a/**/b\"\nallow = [\"anonymous\"]\n";
        assert!(Policy::parse(bad_pattern).is_err());
        assert!(Policy::parse("default_access = \"deny\"").unwrap().rules.is_empty());
    }

    #[test]
    fn shadow_lint() {
        let shadowed = r#"
default_access = "deny"
[[rule]]
path = "/**"
allow = ["anonymous"]
[[rule]]
path = "/private/**"
allow = ["desktop"]
"#;
        let warnings = Policy::parse(shadowed).unwrap().lint();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("unreachable"));
        assert!(Policy::parse(EXAMPLE).unwrap().lint().is_empty());
    }
}
