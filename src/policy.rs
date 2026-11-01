use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
    None,
}

pub fn decision(
    policy: &str,
    principal: Option<&str>,
    action: &str,
    resource: &str,
) -> anyhow::Result<Decision> {
    let policy: Value = serde_json::from_str(policy)?;
    let Some(statements) = policy.get("Statement") else {
        return Ok(Decision::None);
    };
    let statements = statements
        .as_array()
        .cloned()
        .unwrap_or_else(|| vec![statements.clone()]);
    let mut allowed = false;
    for statement in statements {
        if statement.get("Condition").is_some()
            || statement.get("NotAction").is_some()
            || statement.get("NotPrincipal").is_some()
        {
            continue;
        }
        if !matches_value(statement.get("Action"), action)
            || !matches_value(statement.get("Resource"), resource)
            || !matches_principal(statement.get("Principal"), principal)
        {
            continue;
        }
        match statement.get("Effect").and_then(Value::as_str) {
            Some("Deny") => return Ok(Decision::Deny),
            Some("Allow") => allowed = true,
            _ => {}
        }
    }
    Ok(if allowed {
        Decision::Allow
    } else {
        Decision::None
    })
}

pub fn allows_anonymous(policy: &str, action: &str, resource: &str) -> anyhow::Result<bool> {
    Ok(decision(policy, None, action, resource)? == Decision::Allow)
}

pub fn allows_public_read_acl(action: &str) -> bool {
    matches!(action, "s3:GetObject" | "s3:ListBucket")
}

pub fn matches_principal(principal: Option<&Value>, requested: Option<&str>) -> bool {
    match (principal, requested) {
        (Some(Value::String(value)), None) => value == "*",
        (Some(Value::String(value)), Some(requested)) => wildcard_matches(value, requested),
        (Some(Value::Object(object)), requested) => object
            .get("AWS")
            .is_some_and(|value| matches_value(Some(value), requested.unwrap_or("*"))),
        _ => false,
    }
}

fn matches_value(value: Option<&Value>, expected: &str) -> bool {
    match value {
        Some(Value::String(value)) => wildcard_matches(value, expected),
        Some(Value::Array(values)) => values.iter().any(|value| {
            value
                .as_str()
                .is_some_and(|value| wildcard_matches(value, expected))
        }),
        _ => false,
    }
}

fn wildcard_matches(pattern: &str, value: &str) -> bool {
    let mut remainder = value;
    for (index, part) in pattern.split('*').enumerate() {
        if part.is_empty() {
            continue;
        }
        let Some(found) = remainder.find(part) else {
            return false;
        };
        if index == 0 && !pattern.starts_with('*') && found != 0 {
            return false;
        }
        remainder = &remainder[found + part.len()..];
    }
    pattern.ends_with('*') || remainder.is_empty()
}

#[cfg(test)]
mod tests {
    use super::{allows_anonymous, allows_public_read_acl, decision, Decision};

    const OBJECT: &str = "arn:aws:s3:::example/photos/cat.jpg";

    fn policy(statement: &str) -> String {
        format!(r#"{{"Version":"2012-10-17","Statement":{statement}}}"#)
    }

    #[test]
    fn allows_anonymous_public_object_read() {
        let policy = policy(
            r#"{"Effect":"Allow","Principal":"*","Action":"s3:GetObject","Resource":"arn:aws:s3:::example/photos/*"}"#,
        );

        assert!(allows_anonymous(&policy, "s3:GetObject", OBJECT).unwrap());
        assert!(!allows_anonymous(&policy, "s3:PutObject", OBJECT).unwrap());
    }

    #[test]
    fn supports_single_statement_and_action_arrays() {
        let policy = policy(
            r#"{"Effect":"Allow","Principal":"*","Action":["s3:GetObject","s3:ListBucket"],"Resource":"*"}"#,
        );

        assert_eq!(
            decision(&policy, None, "s3:GetObject", OBJECT).unwrap(),
            Decision::Allow
        );
        assert_eq!(
            decision(&policy, None, "s3:ListBucket", "arn:aws:s3:::example").unwrap(),
            Decision::Allow
        );
    }

    #[test]
    fn matches_access_key_principals_and_wildcards() {
        let policy = policy(
            r#"{"Effect":"Allow","Principal":{"AWS":["reader-*","writer-key"]},"Action":"s3:Get*","Resource":"arn:aws:s3:::example/*"}"#,
        );

        assert_eq!(
            decision(&policy, Some("reader-123"), "s3:GetObject", OBJECT).unwrap(),
            Decision::Allow
        );
        assert_eq!(
            decision(&policy, Some("other-key"), "s3:GetObject", OBJECT).unwrap(),
            Decision::None
        );
    }

    #[test]
    fn explicit_deny_takes_precedence_over_allow() {
        let policy = policy(
            r#"[{"Effect":"Allow","Principal":"*","Action":"s3:GetObject","Resource":"*"},{"Effect":"Deny","Principal":"*","Action":"s3:GetObject","Resource":"arn:aws:s3:::example/private/*"}]"#,
        );

        assert_eq!(
            decision(
                &policy,
                None,
                "s3:GetObject",
                "arn:aws:s3:::example/private/key"
            )
            .unwrap(),
            Decision::Deny
        );
        assert_eq!(
            decision(&policy, None, "s3:GetObject", OBJECT).unwrap(),
            Decision::Allow
        );
    }

    #[test]
    fn does_not_grant_unsupported_conditional_or_negative_statements() {
        let policy = policy(
            r#"[{"Effect":"Allow","Principal":"*","Action":"s3:GetObject","Resource":"*","Condition":{"IpAddress":{"aws:SourceIp":"10.0.0.0/8"}}},{"Effect":"Allow","Principal":"*","NotAction":"s3:DeleteObject","Resource":"*"}]"#,
        );

        assert_eq!(
            decision(&policy, None, "s3:GetObject", OBJECT).unwrap(),
            Decision::None
        );
    }

    #[test]
    fn malformed_policy_returns_an_error() {
        assert!(decision("not json", None, "s3:GetObject", OBJECT).is_err());
    }

    #[test]
    fn public_read_acl_maps_only_read_actions() {
        assert!(allows_public_read_acl("s3:GetObject"));
        assert!(allows_public_read_acl("s3:ListBucket"));
        assert!(!allows_public_read_acl("s3:PutObject"));
        assert!(!allows_public_read_acl("s3:DeleteObject"));
    }
}
