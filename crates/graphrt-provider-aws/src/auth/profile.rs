//! Named profiles of the AWS shared files (`~/.aws/credentials`, `~/.aws/config`),
//! as the AWS CLI reads them: `--profile`, else `AWS_PROFILE`, else `default`.
//!
//! Only long-term or exported keys are read (`aws_access_key_id`,
//! `aws_secret_access_key`, `aws_session_token`) plus the profile's `region`.
//! A profile that only names an SSO session, a role to assume or a
//! `credential_process` has no keys here: [`ProfileError`] says to export them
//! (`aws configure export-credentials --profile <name> --format env`).

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use super::sigv4::AwsCredentials;

/// Keys and region of one profile.
#[derive(Clone, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub credentials: Option<AwsCredentials>,
    pub region: Option<String>,
    /// The profile delegates to something this loader does not run (SSO, a role, a process).
    pub delegated: Option<&'static str>,
}

impl fmt::Debug for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Profile")
            .field("name", &self.name)
            .field("credentials", &self.credentials)
            .field("region", &self.region)
            .field("delegated", &self.delegated)
            .finish()
    }
}

/// Why a profile could not be used. Never contains key material.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileError(pub String);

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProfileError {}

/// Where the shared files are: `AWS_SHARED_CREDENTIALS_FILE` / `AWS_CONFIG_FILE`, else `~/.aws/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharedFiles {
    pub credentials: Option<PathBuf>,
    pub config: Option<PathBuf>,
}

impl SharedFiles {
    /// The paths from an environment lookup (`std::env::var` in production).
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let home = lookup("HOME").or_else(|| lookup("USERPROFILE")).filter(|home| !home.is_empty());
        let in_home = |file: &str| home.as_ref().map(|home| Path::new(home).join(".aws").join(file));
        let explicit = |name: &str| lookup(name).filter(|value| !value.is_empty()).map(PathBuf::from);
        Self {
            credentials: explicit("AWS_SHARED_CREDENTIALS_FILE").or_else(|| in_home("credentials")),
            config: explicit("AWS_CONFIG_FILE").or_else(|| in_home("config")),
        }
    }
}

/// Sections of an INI file: section name → key → value (keys lower-cased).
fn parse_ini(text: &str) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut sections: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
            let name = name.trim().to_owned();
            sections.entry(name.clone()).or_default();
            current = Some(name);
            continue;
        }
        let (Some(section), Some((key, value))) = (current.as_ref(), line.split_once('=')) else { continue };
        sections.entry(section.clone()).or_default().insert(key.trim().to_ascii_lowercase(), value.trim().to_owned());
    }
    sections
}

/// The profile `name` from the texts of the credentials and config files.
///
/// @throws ProfileError - the profile is in neither file
pub fn profile_from_texts(
    name: &str,
    credentials_text: Option<&str>,
    config_text: Option<&str>,
) -> Result<Profile, ProfileError> {
    let credentials_file = credentials_text.map(parse_ini).unwrap_or_default();
    let config_file = config_text.map(parse_ini).unwrap_or_default();
    let config_section_name = if name == "default" { "default".to_owned() } else { format!("profile {name}") };
    let from_credentials = credentials_file.get(name);
    let from_config = config_file.get(&config_section_name).or_else(|| config_file.get(name));
    if from_credentials.is_none() && from_config.is_none() {
        return Err(ProfileError(format!(
            "AWS profile {name:?} is not in ~/.aws/credentials or ~/.aws/config (run `aws configure --profile {name}`)"
        )));
    }
    let value = |key: &str| {
        from_credentials
            .and_then(|section| section.get(key))
            .or_else(|| from_config.and_then(|section| section.get(key)))
            .filter(|value| !value.is_empty())
            .cloned()
    };
    let credentials = match (value("aws_access_key_id"), value("aws_secret_access_key")) {
        (Some(access_key_id), Some(secret_access_key)) => {
            Some(AwsCredentials { access_key_id, secret_access_key, session_token: value("aws_session_token") })
        }
        _ => None,
    };
    let delegated = if credentials.is_some() {
        None
    } else if value("sso_session").is_some() || value("sso_start_url").is_some() {
        Some("an SSO login")
    } else if value("role_arn").is_some() {
        Some("a role to assume")
    } else if value("credential_process").is_some() {
        Some("credential_process")
    } else {
        None
    };
    Ok(Profile { name: name.to_owned(), credentials, region: value("region"), delegated })
}

/// Reads the profile `name` from the shared files.
///
/// @throws ProfileError - the profile is in neither file
pub fn load_profile(name: &str, files: &SharedFiles) -> Result<Profile, ProfileError> {
    let read = |path: &Option<PathBuf>| path.as_ref().and_then(|path| std::fs::read_to_string(path).ok());
    let credentials = read(&files.credentials);
    let config = read(&files.config);
    profile_from_texts(name, credentials.as_deref(), config.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CREDENTIALS: &str = "[default]\naws_access_key_id = AKIADEFAULT\naws_secret_access_key = d\n\n\
                               [graphrt]\naws_access_key_id=AKIAGRAPHRT\naws_secret_access_key = s3cr3t\n";
    const CONFIG: &str = "[default]\nregion = us-east-1\n[profile graphrt]\nregion = ap-southeast-1\n\
                          # comment\n[profile sso]\nsso_session = corp\nregion = eu-west-1\n";

    #[test]
    fn a_named_profile_takes_keys_from_credentials_and_region_from_config() {
        let profile = profile_from_texts("graphrt", Some(CREDENTIALS), Some(CONFIG)).unwrap_or_else(|e| panic!("{e}"));
        let credentials = profile.credentials.clone().unwrap_or_else(|| panic!("no keys"));
        assert_eq!(credentials.access_key_id, "AKIAGRAPHRT");
        assert_eq!(profile.region.as_deref(), Some("ap-southeast-1"));
        assert!(!format!("{profile:?}").contains("s3cr3t"), "debug output hides the secret key");
    }

    #[test]
    fn default_uses_the_plain_section_name_in_config() {
        let profile = profile_from_texts("default", Some(CREDENTIALS), Some(CONFIG)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(profile.region.as_deref(), Some("us-east-1"));
    }

    #[test]
    fn an_sso_profile_has_no_keys_and_says_why() {
        let profile = profile_from_texts("sso", Some(CREDENTIALS), Some(CONFIG)).unwrap_or_else(|e| panic!("{e}"));
        assert!(profile.credentials.is_none());
        assert_eq!(profile.delegated, Some("an SSO login"));
    }

    #[test]
    fn a_missing_profile_names_the_fix() {
        let error = profile_from_texts("nope", Some(CREDENTIALS), Some(CONFIG)).err().unwrap_or_else(|| panic!("ok"));
        assert!(error.0.contains("aws configure --profile nope"), "{error}");
    }

    #[test]
    fn shared_files_follow_the_environment() {
        let files = SharedFiles::from_lookup(|name| match name {
            "HOME" => Some("/home/u".into()),
            "AWS_CONFIG_FILE" => Some("/etc/aws.conf".into()),
            _ => None,
        });
        assert_eq!(files.credentials, Some(PathBuf::from("/home/u/.aws/credentials")));
        assert_eq!(files.config, Some(PathBuf::from("/etc/aws.conf")));
    }
}
