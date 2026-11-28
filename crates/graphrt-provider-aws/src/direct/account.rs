//! The account side of a direct deploy: who the credentials are (STS), the
//! function's execution role (IAM) and its log group (CloudWatch Logs).

use serde_json::json;

use super::{DirectError, ErrorCode};
use crate::api::ProviderError;
use crate::auth::sigv4::uri_encode;
use crate::auth::xml_text;
use crate::rest::{AwsRequest, AwsRestClient};

/// AWS's managed policy that lets a function write its logs.
pub(crate) const BASIC_EXECUTION_POLICY: &str = "arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole";
/// Days CloudWatch keeps the function's logs.
pub(crate) const LOG_RETENTION_DAYS: u32 = 14;
const TRUST_POLICY: &str = r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"lambda.amazonaws.com"},"Action":"sts:AssumeRole"}]}"#;

/// Calls an AWS Query API (STS, IAM): form-encoded `Action=...`, XML answer.
pub(crate) async fn query_call(
    client: &AwsRestClient,
    action: &str,
    version: &str,
    parameters: &[(&str, &str)],
) -> Result<String, ProviderError> {
    let mut pairs = vec![format!("Action={action}"), format!("Version={version}")];
    pairs.extend(
        parameters.iter().map(|(key, value)| format!("{}={}", uri_encode(key, false), uri_encode(value, false))),
    );
    let body = pairs.join("&");
    let headers = [("content-type", "application/x-www-form-urlencoded; charset=utf-8")];
    let request = AwsRequest {
        method: "POST",
        path: "/",
        query: &[],
        headers: &headers,
        body: body.as_bytes(),
        idempotent: true,
    };
    let response = client.send(&request).await?;
    let bytes =
        response.bytes().await.map_err(|error| ProviderError::transient(format!("{action} response body: {error}")))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The account id of the credentials (`sts:GetCallerIdentity`, needs no permission).
pub(crate) async fn account_id(sts: &AwsRestClient) -> Result<String, DirectError> {
    let document = query_call(sts, "GetCallerIdentity", "2011-06-15", &[])
        .await
        .map_err(|error| DirectError::from_aws(error, "check the AWS credentials"))?;
    xml_text(&document, "Account")
        .filter(|account| !account.is_empty())
        .ok_or_else(|| DirectError::new(ErrorCode::AwsApi, "STS did not say which account the credentials belong to"))
}

/// The ARN of role `name`, if it exists.
pub(crate) async fn role_arn(iam: &AwsRestClient, name: &str) -> Result<Option<String>, DirectError> {
    match query_call(iam, "GetRole", "2010-05-08", &[("RoleName", name)]).await {
        Ok(document) => Ok(xml_text(&document, "Arn")),
        Err(error) if error.message.starts_with("NoSuchEntity") => Ok(None),
        Err(error) => Err(DirectError::from_aws(error, "read the function's IAM role")),
    }
}

/// Creates the execution role `name` (logs only) unless it exists; returns its ARN
/// and whether it was created now (a new role takes a few seconds to be usable).
pub(crate) async fn ensure_role(
    iam: &AwsRestClient,
    name: &str,
    deployment: &str,
) -> Result<(String, bool), DirectError> {
    if let Some(arn) = role_arn(iam, name).await? {
        return Ok((arn, false));
    }
    let description = format!("graphrt deploy --direct: execution role of {deployment}");
    let parameters = [
        ("RoleName", name),
        ("AssumeRolePolicyDocument", TRUST_POLICY),
        ("Description", description.as_str()),
        ("Tags.member.1.Key", "graphrt:deployment"),
        ("Tags.member.1.Value", deployment),
    ];
    let created = match query_call(iam, "CreateRole", "2010-05-08", &parameters).await {
        Ok(document) => xml_text(&document, "Arn"),
        Err(error) if error.message.starts_with("EntityAlreadyExists") => role_arn(iam, name).await?,
        Err(error) => return Err(DirectError::from_aws(error, "create the function's IAM role")),
    };
    let arn = created.ok_or_else(|| DirectError::new(ErrorCode::AwsApi, "IAM gave no role ARN"))?;
    query_call(iam, "AttachRolePolicy", "2010-05-08", &[("RoleName", name), ("PolicyArn", BASIC_EXECUTION_POLICY)])
        .await
        .map_err(|error| DirectError::from_aws(error, "attach the logging policy to the function's role"))?;
    Ok((arn, true))
}

/// Name of the inline policy that lets the role use the store bucket.
pub(crate) const STORE_POLICY: &str = "graphrt-store";

/// Lets role `name` use the store bucket (an inline policy, replaced each time).
pub(crate) async fn put_store_policy(iam: &AwsRestClient, name: &str, document: &str) -> Result<(), DirectError> {
    query_call(
        iam,
        "PutRolePolicy",
        "2010-05-08",
        &[("RoleName", name), ("PolicyName", STORE_POLICY), ("PolicyDocument", document)],
    )
    .await
    .map(|_| ())
    .map_err(|error| DirectError::from_aws(error, "let the function's role use the store bucket"))
}

/// Removes the store policy of role `name` (a missing one is fine).
pub(crate) async fn delete_store_policy(iam: &AwsRestClient, name: &str) -> Result<(), DirectError> {
    match query_call(iam, "DeleteRolePolicy", "2010-05-08", &[("RoleName", name), ("PolicyName", STORE_POLICY)]).await {
        Ok(_) => Ok(()),
        Err(error)
            if error.message.starts_with("NoSuchEntity") || error.kind == crate::api::ProviderErrorKind::NotFound =>
        {
            Ok(())
        }
        Err(error) => Err(DirectError::from_aws(error, "remove the store policy of the function's role")),
    }
}

/// Deletes role `name` if graphrt made it (tag `graphrt:deployment`); `false` when absent or not ours.
pub(crate) async fn delete_role(iam: &AwsRestClient, name: &str) -> Result<bool, DirectError> {
    let tags = match query_call(iam, "ListRoleTags", "2010-05-08", &[("RoleName", name)]).await {
        Ok(document) => document,
        Err(error) if error.message.starts_with("NoSuchEntity") => return Ok(false),
        Err(error) => return Err(DirectError::from_aws(error, "read the function's IAM role")),
    };
    if !tags.contains("<Key>graphrt:deployment</Key>") {
        return Ok(false);
    }
    let context = "delete the function's IAM role";
    delete_store_policy(iam, name).await?;
    match query_call(
        iam,
        "DetachRolePolicy",
        "2010-05-08",
        &[("RoleName", name), ("PolicyArn", BASIC_EXECUTION_POLICY)],
    )
    .await
    {
        Ok(_) => {}
        Err(error) if error.message.starts_with("NoSuchEntity") => {}
        Err(error) => return Err(DirectError::from_aws(error, context)),
    }
    match query_call(iam, "DeleteRole", "2010-05-08", &[("RoleName", name)]).await {
        Ok(_) => Ok(true),
        Err(error) if error.message.starts_with("NoSuchEntity") => Ok(false),
        Err(error) => Err(DirectError::from_aws(error, context)),
    }
}

/// Calls a CloudWatch Logs action, whose successful answers are often an empty body.
async fn logs_call(logs: &AwsRestClient, action: &str, body: &serde_json::Value) -> Result<(), ProviderError> {
    let payload =
        serde_json::to_vec(body).map_err(|error| ProviderError::fatal(format!("cannot encode request: {error}")))?;
    let target = format!("Logs_20140328.{action}");
    let headers = [("content-type", "application/x-amz-json-1.1"), ("x-amz-target", target.as_str())];
    let request =
        AwsRequest { method: "POST", path: "/", query: &[], headers: &headers, body: &payload, idempotent: true };
    logs.send(&request).await.map(|_| ())
}

/// Creates the function's log group with a retention, so logs do not pile up.
pub(crate) async fn ensure_log_group(logs: &AwsRestClient, name: &str, deployment: &str) -> Result<(), DirectError> {
    let context = "create the function's log group";
    let body = json!({ "logGroupName": name, "tags": { "graphrt:deployment": deployment } });
    match logs_call(logs, "CreateLogGroup", &body).await {
        Ok(_) => {}
        Err(error) if error.message.starts_with("ResourceAlreadyExistsException") => {}
        Err(error) => return Err(DirectError::from_aws(error, context)),
    }
    let retention = json!({ "logGroupName": name, "retentionInDays": LOG_RETENTION_DAYS });
    logs_call(logs, "PutRetentionPolicy", &retention).await.map_err(|error| DirectError::from_aws(error, context))
}

/// Deletes the log group; `false` when it did not exist.
pub(crate) async fn delete_log_group(logs: &AwsRestClient, name: &str) -> Result<bool, DirectError> {
    match logs_call(logs, "DeleteLogGroup", &json!({ "logGroupName": name })).await {
        Ok(_) => Ok(true),
        Err(error) if error.message.starts_with("ResourceNotFoundException") => Ok(false),
        Err(error) => Err(DirectError::from_aws(error, "delete the function's log group")),
    }
}
