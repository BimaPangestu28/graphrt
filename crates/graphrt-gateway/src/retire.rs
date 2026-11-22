//! Retiring versions on the platform once their 7-day pool expires (decision 6).
//!
//! The gateway always answers `410 version_retired` from its own records; a
//! [`VersionRetirer`] additionally frees what the platform keeps for an old
//! version:
//!
//! | Provider | What retirement does | Needs |
//! | --- | --- | --- |
//! | Cloud Run | drops the version's traffic tag (`services.patch?updateMask=traffic`) | `deploy-cloud-run`, `run.developer` on the service |
//! | Container Apps | deactivates revisions past the retention and drops their labels | `deploy-aca`, write role on the app |
//! | Fly | destroys the version's Machine pool | `deploy-fly`, the app's token |
//! | Kubernetes, ECS, Lambda | nothing: the closed pool already scaled the version to zero and no platform object costs anything | – |
//!
//! Without the deploy feature the gateway logs that the platform objects stay.

use graphrt_provider_api::{BoxFuture, DeploymentRef};

/// Frees the platform resources of retired versions.
pub trait VersionRetirer: Send + Sync {
    /// Retires `versions` (all of one deployment); returns a note for the audit log.
    fn retire<'a>(&'a self, versions: &'a [DeploymentRef]) -> BoxFuture<'a, Result<String, String>>;
}

/// Cloud Run: drop the traffic tags of retired versions.
#[cfg(feature = "deploy-cloud-run")]
#[derive(Debug)]
pub struct CloudRunRetirer(pub graphrt_gcp::cloud_run::CloudRunDeployDriver);

#[cfg(feature = "deploy-cloud-run")]
impl VersionRetirer for CloudRunRetirer {
    fn retire<'a>(&'a self, versions: &'a [DeploymentRef]) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let Some(first) = versions.first() else { return Ok(String::new()) };
            let names: Vec<String> = versions.iter().map(|version| version.version.clone()).collect();
            self.0.retire_versions(first, &names).await.map_err(|error| error.to_string())?;
            Ok(format!("removed the traffic tags of {} versions", names.len()))
        })
    }
}

/// Container Apps: deactivate revisions past the retention window.
#[cfg(feature = "deploy-aca")]
#[derive(Debug)]
pub struct AcaRetirer(pub graphrt_provider_azure::aca::AcaProvider);

#[cfg(feature = "deploy-aca")]
impl VersionRetirer for AcaRetirer {
    fn retire<'a>(&'a self, versions: &'a [DeploymentRef]) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let Some(first) = versions.first() else { return Ok(String::new()) };
            let app = if first.target.is_empty() { first.name.as_str() } else { first.target.as_str() };
            let now = graphrt_core::store::now_ms() / 1000;
            let retired = self.0.retire_old_versions(app, now).await.map_err(|error| error.to_string())?;
            Ok(format!("deactivated revisions {retired:?}"))
        })
    }
}

/// Fly: destroy the Machine pool of each retired version, with the token of its app.
#[cfg(feature = "deploy-fly")]
#[derive(Debug)]
pub struct FlyRetirer(pub std::collections::BTreeMap<String, graphrt_provider_fly::FlyDeployDriver>);

#[cfg(feature = "deploy-fly")]
impl VersionRetirer for FlyRetirer {
    fn retire<'a>(&'a self, versions: &'a [DeploymentRef]) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let mut destroyed = 0;
            for version in versions {
                let driver = self
                    .0
                    .get(&version.target)
                    .ok_or_else(|| format!("fly app `{}` is not configured", version.target))?;
                destroyed += driver.retire_version(version).await.map_err(|error| error.to_string())?;
            }
            Ok(format!("destroyed {destroyed} Machines"))
        })
    }
}
