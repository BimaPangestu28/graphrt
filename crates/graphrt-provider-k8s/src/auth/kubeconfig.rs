//! Kubeconfig files (outside a cluster): `clusters`, `users`, `contexts`,
//! `current-context`. Supported user credentials: `token`, `tokenFile`,
//! `client-certificate(-data)` + `client-key(-data)`, `exec`. The legacy
//! `auth-provider` plugins are rejected with a pointer to their exec plugin.
//! `insecure-skip-tls-verify` is rejected rather than honoured.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{AuthError, ClusterConfig, ExecConfig, ExecTokenFetcher, SecretString, StaticToken, TokenFile, cached};
use base64::Engine as _;

/// Which kubeconfig and context to use.
#[derive(Debug, Clone, Default)]
pub struct KubeconfigOptions {
    /// Explicit file; default `$KUBECONFIG` (all listed files merged), then `~/.kube/config`.
    pub path: Option<PathBuf>,
    /// Context name; default `current-context`.
    pub context: Option<String>,
}

#[derive(Debug, Default)]
struct KubeconfigFile {
    clusters: Vec<Named<ClusterEntry>>,
    users: Vec<Named<UserEntry>>,
    contexts: Vec<Named<ContextEntry>>,
    current_context: Option<String>,
}

#[derive(Debug)]
struct Named<T> {
    name: String,
    value: Option<T>,
}

#[derive(Debug, Default)]
struct ClusterEntry {
    server: Option<String>,
    certificate_authority: Option<String>,
    certificate_authority_data: Option<String>,
    insecure_skip_tls_verify: bool,
    tls_server_name: Option<String>,
    proxy_url: Option<String>,
}

#[derive(Debug, Default)]
struct UserEntry {
    token: Option<String>,
    token_file: Option<String>,
    client_certificate: Option<String>,
    client_certificate_data: Option<String>,
    client_key: Option<String>,
    client_key_data: Option<String>,
    exec: Option<ExecEntry>,
    auth_provider: Option<AuthProviderEntry>,
    username: Option<String>,
}

#[derive(Debug)]
struct AuthProviderEntry {
    name: String,
}

#[derive(Debug, Default)]
struct ExecEntry {
    api_version: String,
    command: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    provide_cluster_info: bool,
    interactive_mode: Option<String>,
}

#[derive(Debug, Default)]
struct ContextEntry {
    cluster: String,
    user: Option<String>,
    namespace: Option<String>,
}

// The kubeconfig is read by walking a `serde_json::Value` instead of deriving
// `Deserialize` for each struct: one generic instantiation (the YAML
// deserializer into `Value`, which the gateway already links for its own
// config) instead of one per struct keeps the feature small (plan §9).
type Json = serde_json::Value;

fn text(value: &Json, key: &str) -> Option<String> {
    value.get(key).and_then(Json::as_str).map(str::to_owned)
}

fn texts(value: &Json, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Json::as_array)
        .map(|items| items.iter().filter_map(Json::as_str).map(str::to_owned).collect())
        .unwrap_or_default()
}

fn flag(value: &Json, key: &str) -> bool {
    value.get(key).and_then(Json::as_bool).unwrap_or(false)
}

/// `[{name, <inner>: {...}}]` lists; entries without a name are invalid.
fn named<T>(root: &Json, list: &str, inner: &str, parse: fn(&Json) -> T) -> Result<Vec<Named<T>>, String> {
    let Some(items) = root.get(list).filter(|items| !items.is_null()) else { return Ok(Vec::new()) };
    let items = items.as_array().ok_or_else(|| format!("{list} must be a list"))?;
    items
        .iter()
        .map(|item| {
            let name = text(item, "name").ok_or_else(|| format!("an entry of {list} has no name"))?;
            Ok(Named { name, value: item.get(inner).filter(|value| value.is_object()).map(parse) })
        })
        .collect()
}

fn cluster_entry(value: &Json) -> ClusterEntry {
    ClusterEntry {
        server: text(value, "server"),
        certificate_authority: text(value, "certificate-authority"),
        certificate_authority_data: text(value, "certificate-authority-data"),
        insecure_skip_tls_verify: flag(value, "insecure-skip-tls-verify"),
        tls_server_name: text(value, "tls-server-name"),
        proxy_url: text(value, "proxy-url"),
    }
}

fn exec_entry(value: &Json) -> ExecEntry {
    let env = value
        .get("env")
        .and_then(Json::as_array)
        .map(|items| items.iter().filter_map(|item| Some((text(item, "name")?, text(item, "value")?))).collect())
        .unwrap_or_default();
    ExecEntry {
        api_version: text(value, "apiVersion").unwrap_or_default(),
        command: text(value, "command").unwrap_or_default(),
        args: texts(value, "args"),
        env,
        provide_cluster_info: flag(value, "provideClusterInfo"),
        interactive_mode: text(value, "interactiveMode"),
    }
}

fn user_entry(value: &Json) -> UserEntry {
    UserEntry {
        token: text(value, "token"),
        token_file: text(value, "tokenFile"),
        client_certificate: text(value, "client-certificate"),
        client_certificate_data: text(value, "client-certificate-data"),
        client_key: text(value, "client-key"),
        client_key_data: text(value, "client-key-data"),
        exec: value.get("exec").filter(|exec| exec.is_object()).map(exec_entry),
        auth_provider: value
            .get("auth-provider")
            .filter(|provider| provider.is_object())
            .map(|provider| AuthProviderEntry { name: text(provider, "name").unwrap_or_default() }),
        username: text(value, "username"),
    }
}

fn context_entry(value: &Json) -> ContextEntry {
    ContextEntry {
        cluster: text(value, "cluster").unwrap_or_default(),
        user: text(value, "user"),
        namespace: text(value, "namespace"),
    }
}

/// Load the kubeconfig named by `options`, `$KUBECONFIG` or `~/.kube/config`.
///
/// @throws AuthError::Kubeconfig - missing file, bad YAML, unknown context
pub fn kubeconfig_from_env(options: &KubeconfigOptions) -> Result<ClusterConfig, AuthError> {
    let paths = kubeconfig_paths(options)?;
    let mut merged = KubeconfigFile::default();
    for path in &paths {
        let text = std::fs::read_to_string(path)
            .map_err(|error| AuthError::Kubeconfig(format!("read {}: {error}", path.display())))?;
        merge(&mut merged, parse(&text, path)?, path.parent().unwrap_or(Path::new(".")));
    }
    // Relative paths were made absolute per file during the merge.
    resolve(merged, Path::new("/"), options.context.as_deref())
}

/// Parse a kubeconfig document; relative file paths resolve against `base_dir`.
pub fn kubeconfig_from_str(text: &str, base_dir: &Path, context: Option<&str>) -> Result<ClusterConfig, AuthError> {
    resolve(parse(text, base_dir)?, base_dir, context)
}

fn parse(source: &str, origin: &Path) -> Result<KubeconfigFile, AuthError> {
    let invalid = |error: String| AuthError::Kubeconfig(format!("{}: {error}", origin.display()));
    let root: Json = serde_saphyr::from_str(source).map_err(|error| invalid(error.to_string()))?;
    if root.is_null() {
        return Ok(KubeconfigFile::default());
    }
    if !root.is_object() {
        return Err(invalid("not a kubeconfig mapping".to_owned()));
    }
    Ok(KubeconfigFile {
        clusters: named(&root, "clusters", "cluster", cluster_entry).map_err(invalid)?,
        users: named(&root, "users", "user", user_entry).map_err(invalid)?,
        contexts: named(&root, "contexts", "context", context_entry).map_err(invalid)?,
        current_context: text(&root, "current-context"),
    })
}

fn kubeconfig_paths(options: &KubeconfigOptions) -> Result<Vec<PathBuf>, AuthError> {
    if let Some(path) = &options.path {
        return Ok(vec![path.clone()]);
    }
    if let Some(list) = std::env::var_os("KUBECONFIG").filter(|list| !list.is_empty()) {
        let paths: Vec<PathBuf> = std::env::split_paths(&list).filter(|path| path.is_file()).collect();
        if !paths.is_empty() {
            return Ok(paths);
        }
    }
    let home = std::env::var_os("HOME").ok_or_else(|| AuthError::Kubeconfig("no $KUBECONFIG and no $HOME".into()))?;
    Ok(vec![PathBuf::from(home).join(".kube").join("config")])
}

/// Merge `next` into `merged` the way kubectl does: the first file defining a
/// name or `current-context` wins.
fn merge(merged: &mut KubeconfigFile, mut next: KubeconfigFile, base_dir: &Path) {
    absolutize(&mut next, base_dir);
    for cluster in next.clusters {
        if !merged.clusters.iter().any(|existing| existing.name == cluster.name) {
            merged.clusters.push(cluster);
        }
    }
    for user in next.users {
        if !merged.users.iter().any(|existing| existing.name == user.name) {
            merged.users.push(user);
        }
    }
    for context in next.contexts {
        if !merged.contexts.iter().any(|existing| existing.name == context.name) {
            merged.contexts.push(context);
        }
    }
    if merged.current_context.is_none() {
        merged.current_context = next.current_context;
    }
}

fn absolutize(file: &mut KubeconfigFile, base_dir: &Path) {
    let fix = |path: &mut Option<String>| {
        if let Some(value) = path.as_mut() {
            *value = base_dir.join(&*value).to_string_lossy().into_owned();
        }
    };
    for cluster in file.clusters.iter_mut().filter_map(|named| named.value.as_mut()) {
        fix(&mut cluster.certificate_authority);
    }
    for user in file.users.iter_mut().filter_map(|named| named.value.as_mut()) {
        fix(&mut user.token_file);
        fix(&mut user.client_certificate);
        fix(&mut user.client_key);
        if let Some(exec) = user.exec.as_mut().filter(|exec| exec.command.contains('/')) {
            exec.command = base_dir.join(&exec.command).to_string_lossy().into_owned();
        }
    }
}

fn resolve(mut file: KubeconfigFile, base_dir: &Path, context: Option<&str>) -> Result<ClusterConfig, AuthError> {
    absolutize(&mut file, base_dir);
    let context_name = context
        .map(str::to_owned)
        .or(file.current_context.clone())
        .ok_or_else(|| AuthError::Kubeconfig("no context selected and no current-context".into()))?;
    let context = find(&file.contexts, &context_name, "context")?;
    let cluster = find(&file.clusters, &context.cluster, "cluster")?;
    let user = match context.user.as_deref().filter(|user| !user.is_empty()) {
        Some(user_name) => Some(find(&file.users, user_name, "user")?),
        None => None,
    };
    let server = cluster
        .server
        .clone()
        .filter(|server| !server.is_empty())
        .ok_or_else(|| AuthError::Kubeconfig(format!("cluster {:?} has no server", context.cluster)))?;
    check_cluster_options(cluster)?;
    let ca_pem = read_pem(cluster.certificate_authority_data.as_deref(), cluster.certificate_authority.as_deref())?;
    let mut config = ClusterConfig::new(server.clone());
    config.ca_pem = ca_pem;
    config.namespace = context.namespace.clone().filter(|namespace| !namespace.is_empty());
    if let Some(user) = user {
        apply_user(&mut config, user, &server)?;
    }
    Ok(config)
}

fn find<'a, T>(entries: &'a [Named<T>], name: &str, kind: &str) -> Result<&'a T, AuthError> {
    entries
        .iter()
        .find(|entry| entry.name == name)
        .and_then(|entry| entry.value.as_ref())
        .ok_or_else(|| AuthError::Kubeconfig(format!("{kind} {name:?} not found")))
}

fn check_cluster_options(cluster: &ClusterEntry) -> Result<(), AuthError> {
    if cluster.insecure_skip_tls_verify {
        return Err(AuthError::Unsupported(
            "insecure-skip-tls-verify: set certificate-authority(-data) instead".into(),
        ));
    }
    if cluster.tls_server_name.is_some() {
        return Err(AuthError::Unsupported("tls-server-name".into()));
    }
    if cluster.proxy_url.is_some() {
        return Err(AuthError::Unsupported("proxy-url".into()));
    }
    Ok(())
}

fn apply_user(config: &mut ClusterConfig, user: &UserEntry, server: &str) -> Result<(), AuthError> {
    if let Some(provider) = &user.auth_provider {
        return Err(AuthError::Unsupported(format!(
            "auth-provider {:?}: switch the kubeconfig to its exec plugin (gke-gcloud-auth-plugin, kubelogin)",
            provider.name
        )));
    }
    if user.username.is_some() {
        return Err(AuthError::Unsupported("basic auth (username/password)".into()));
    }
    let cert = read_pem(user.client_certificate_data.as_deref(), user.client_certificate.as_deref())?;
    let key = read_pem(user.client_key_data.as_deref(), user.client_key.as_deref())?;
    match (cert, key) {
        (Some(mut cert), Some(key)) => {
            cert.push(b'\n');
            cert.extend_from_slice(&key);
            let identity = String::from_utf8(cert)
                .map_err(|_| AuthError::Kubeconfig("client certificate / key is not PEM text".into()))?;
            config.client_identity_pem = Some(SecretString::new(identity));
        }
        (None, None) => {}
        _ => return Err(AuthError::Kubeconfig("client-certificate and client-key must be set together".into())),
    }
    if let Some(token) = user.token.as_deref().filter(|token| !token.is_empty()) {
        config.token_source = Some(Arc::new(StaticToken(SecretString::new(token))));
    } else if let Some(path) = &user.token_file {
        config.token_source = Some(cached(TokenFile(PathBuf::from(path))));
    } else if let Some(exec) = &user.exec {
        config.token_source = Some(cached(exec_fetcher(exec, server)?));
    }
    Ok(())
}

fn exec_fetcher(exec: &ExecEntry, server: &str) -> Result<ExecTokenFetcher, AuthError> {
    if exec.command.is_empty() || exec.api_version.is_empty() {
        return Err(AuthError::Kubeconfig("exec plugin needs apiVersion and command".into()));
    }
    if exec.interactive_mode.as_deref() == Some("Always") {
        return Err(AuthError::Unsupported("exec plugin with interactiveMode: Always (the gateway has no TTY)".into()));
    }
    ExecTokenFetcher::new(ExecConfig {
        api_version: exec.api_version.clone(),
        command: exec.command.clone(),
        args: exec.args.clone(),
        env: exec.env.clone(),
        cluster_server: exec.provide_cluster_info.then(|| server.to_owned()),
    })
}

/// Inline base64 data wins over a file path, as in kubectl.
fn read_pem(data: Option<&str>, path: Option<&str>) -> Result<Option<Vec<u8>>, AuthError> {
    if let Some(data) = data.filter(|data| !data.is_empty()) {
        let compact: String = data.chars().filter(|char| !char.is_whitespace()).collect();
        return base64::engine::general_purpose::STANDARD
            .decode(compact)
            .map(Some)
            .map_err(|_| AuthError::Kubeconfig("invalid base64 in *-data field".into()));
    }
    match path.filter(|path| !path.is_empty()) {
        Some(path) => {
            std::fs::read(path).map(Some).map_err(|error| AuthError::Kubeconfig(format!("read {path}: {error}")))
        }
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KIND_STYLE: &str = r#"
apiVersion: v1
kind: Config
current-context: kind-graphrt
clusters:
- name: kind-graphrt
  cluster:
    server: https://127.0.0.1:6443
    certificate-authority-data: LS0tLS1CRUdJTiBDRVJUSUZJQ0FURS0tLS0tCg==
contexts:
- name: kind-graphrt
  context:
    cluster: kind-graphrt
    user: kind-graphrt
    namespace: agents
- name: token-ctx
  context:
    cluster: kind-graphrt
    user: token-user
- name: exec-ctx
  context:
    cluster: kind-graphrt
    user: exec-user
users:
- name: kind-graphrt
  user:
    client-certificate-data: Q0VSVAo=
    client-key-data: S0VZCg==
- name: token-user
  user:
    token: abc.def
- name: exec-user
  user:
    exec:
      apiVersion: client.authentication.k8s.io/v1beta1
      command: aws
      args: ["eks", "get-token", "--cluster-name", "prod"]
      env:
      - name: AWS_PROFILE
        value: prod
      provideClusterInfo: true
"#;

    #[test]
    fn current_context_with_client_certificate() {
        let config = kubeconfig_from_str(KIND_STYLE, Path::new("/home/me/.kube"), None).unwrap();
        assert_eq!(config.server, "https://127.0.0.1:6443");
        assert_eq!(config.ca_pem.as_deref(), Some(&b"-----BEGIN CERTIFICATE-----\n"[..]));
        assert_eq!(config.namespace.as_deref(), Some("agents"));
        assert_eq!(config.client_identity_pem.unwrap().expose(), "CERT\n\nKEY\n");
        assert!(config.token_source.is_none());
    }

    #[tokio::test]
    async fn token_and_exec_users() {
        let config = kubeconfig_from_str(KIND_STYLE, Path::new("/"), Some("token-ctx")).unwrap();
        assert_eq!(config.token_source.unwrap().token(false).await.unwrap().expose(), "abc.def");
        // The exec user's plugin is `aws`, run with KUBERNETES_EXEC_INFO; here only resolution is checked.
        let exec = kubeconfig_from_str(KIND_STYLE, Path::new("/"), Some("exec-ctx")).unwrap();
        assert!(exec.token_source.is_some());
    }

    #[test]
    fn relative_paths_resolve_against_the_kubeconfig_directory() {
        let text = r#"
current-context: c
clusters: [{name: k, cluster: {server: "https://k", certificate-authority: ca.crt}}]
users: [{name: u, user: {tokenFile: token, exec: null}}]
contexts: [{name: c, context: {cluster: k, user: u}}]
"#;
        let error = kubeconfig_from_str(text, Path::new("/nonexistent/kube"), None).unwrap_err();
        let expected = Path::new("/nonexistent/kube").join("ca.crt");
        assert!(error.to_string().contains(&expected.display().to_string()), "{error}");
    }

    #[test]
    fn rejects_insecure_and_legacy_settings() {
        let insecure = r#"
current-context: c
clusters: [{name: k, cluster: {server: "https://k", insecure-skip-tls-verify: true}}]
contexts: [{name: c, context: {cluster: k}}]
"#;
        assert!(matches!(kubeconfig_from_str(insecure, Path::new("/"), None), Err(AuthError::Unsupported(_))));
        let legacy = r#"
current-context: c
clusters: [{name: k, cluster: {server: "https://k"}}]
users: [{name: u, user: {auth-provider: {name: gcp}}}]
contexts: [{name: c, context: {cluster: k, user: u}}]
"#;
        let error = kubeconfig_from_str(legacy, Path::new("/"), None).unwrap_err();
        assert!(error.to_string().contains("gke-gcloud-auth-plugin"), "{error}");
        let interactive = r#"
current-context: c
clusters: [{name: k, cluster: {server: "https://k"}}]
users: [{name: u, user: {exec: {apiVersion: client.authentication.k8s.io/v1, command: x, interactiveMode: Always}}}]
contexts: [{name: c, context: {cluster: k, user: u}}]
"#;
        assert!(matches!(kubeconfig_from_str(interactive, Path::new("/"), None), Err(AuthError::Unsupported(_))));
    }

    #[test]
    fn missing_context_or_half_a_client_cert_is_an_error() {
        assert!(kubeconfig_from_str(KIND_STYLE, Path::new("/"), Some("nope")).is_err());
        let half = r#"
current-context: c
clusters: [{name: k, cluster: {server: "https://k"}}]
users: [{name: u, user: {client-certificate-data: Q0VSVAo=}}]
contexts: [{name: c, context: {cluster: k, user: u}}]
"#;
        assert!(kubeconfig_from_str(half, Path::new("/"), None).is_err());
    }
}
