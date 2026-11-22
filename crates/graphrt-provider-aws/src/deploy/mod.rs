//! Deploy-time drivers (`graphrt deploy --target ecs-fargate|lambda`).
//!
//! Behind the `deploy-ecs` / `deploy-lambda` features (`deploy` enables both) so
//! the gateway's run-time build does not link them. They run with deploy-time
//! credentials (CI / operator), never with the gateway's run-time role.
//!
//! Secrets stay references: on ECS they become `secrets[].valueFrom` resolved
//! by the task execution role at start; Lambda has no secret references, so
//! the Lambda driver refuses secrets unless the operator opts into resolving
//! them at deploy time into KMS-encrypted environment variables.

#[cfg(feature = "deploy-ecs")]
pub mod ecs;
#[cfg(feature = "deploy-lambda")]
pub mod lambda;
mod settings;

pub use self::settings::Settings;

use crate::api::{DeployError, SecretRef};

/// Whether `reference` is a Secrets Manager secret or an SSM parameter ARN
/// (any partition: `aws`, `aws-cn`, `aws-us-gov`).
pub fn is_secret_arn(reference: &str) -> bool {
    let mut parts = reference.splitn(6, ':');
    let (Some("arn"), Some(partition), Some(service), Some(region), Some(account), Some(resource)) =
        (parts.next(), parts.next(), parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let resource_ok = match service {
        "secretsmanager" => resource.starts_with("secret:"),
        "ssm" => resource.starts_with("parameter/"),
        _ => false,
    };
    partition.starts_with("aws")
        && !region.is_empty()
        && account.len() == 12
        && account.bytes().all(|b| b.is_ascii_digit())
        && resource_ok
}

/// Check every secret is an ARN reference with a valid environment name.
///
/// @throws `DeployError::invalid` naming the first bad secret (never its value).
pub fn validate_secret_refs(secrets: &[SecretRef]) -> Result<(), DeployError> {
    for secret in secrets {
        if !is_env_name(&secret.env) {
            return Err(DeployError::invalid(format!("secret env name {:?} is not a valid variable name", secret.env)));
        }
        if !is_secret_arn(&secret.reference) {
            return Err(DeployError::invalid(format!(
                "secret {} must reference a Secrets Manager or SSM parameter ARN \
                 (arn:aws:secretsmanager:…:secret:… or arn:aws:ssm:…:parameter/…)",
                secret.env
            )));
        }
    }
    Ok(())
}

/// `[A-Za-z_][A-Za-z0-9_]*`.
pub fn is_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_arns() {
        assert!(is_secret_arn("arn:aws:secretsmanager:eu-west-1:123456789012:secret:openai-AbCdEf"));
        assert!(is_secret_arn("arn:aws:secretsmanager:eu-west-1:123456789012:secret:llm-AbCdEf:openai::"));
        assert!(is_secret_arn("arn:aws:ssm:eu-west-1:123456789012:parameter/graphrt/openai"));
        assert!(is_secret_arn("arn:aws-cn:ssm:cn-north-1:123456789012:parameter/x"));
        assert!(!is_secret_arn("sk-live-plaintext"));
        assert!(!is_secret_arn("arn:aws:s3:::bucket/key"));
        assert!(!is_secret_arn("arn:aws:ssm:eu-west-1:12345:parameter/x"));
        assert!(!is_secret_arn("arn:aws:secretsmanager:eu-west-1:123456789012:other:x"));
    }

    #[test]
    fn env_names_and_refs() {
        assert!(is_env_name("OPENAI_API_KEY") && is_env_name("_x1"));
        assert!(!is_env_name("1A") && !is_env_name("A-B") && !is_env_name(""));
        let bad = SecretRef { env: "OPENAI".into(), reference: "sk-plaintext-value".into() };
        let error = validate_secret_refs(&[bad]).unwrap_err();
        assert!(!error.message.contains("sk-plaintext-value"));
    }
}
