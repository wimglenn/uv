use std::path::Path;

use anyhow::Result;
use tracing::debug;

use pep508_rs::{Requirement, VersionOrUrl};
use puffin_distribution::{CachedDistribution, InstalledDistribution};
use puffin_interpreter::Virtualenv;
use puffin_package::package_name::PackageName;

use crate::url_index::UrlIndex;
use crate::{RegistryIndex, SitePackages};

#[derive(Debug, Default)]
pub struct PartitionedRequirements {
    /// The distributions that are not already installed in the current environment, but are
    /// available in the local cache.
    pub local: Vec<CachedDistribution>,

    /// The distributions that are not already installed in the current environment, and are
    /// not available in the local cache.
    pub remote: Vec<Requirement>,

    /// The distributions that are already installed in the current environment, and are
    /// _not_ necessary to satisfy the requirements.
    pub extraneous: Vec<InstalledDistribution>,
}

impl PartitionedRequirements {
    /// Partition a set of requirements into those that should be linked from the cache, those that
    /// need to be downloaded, and those that should be removed.
    pub fn try_from_requirements(
        requirements: &[Requirement],
        cache: Option<&Path>,
        venv: &Virtualenv,
    ) -> Result<Self> {
        // Index all the already-installed packages in site-packages.
        let mut site_packages = SitePackages::try_from_executable(venv)?;

        // Index all the already-downloaded wheels in the cache.
        let registry_index = if let Some(cache) = cache {
            RegistryIndex::try_from_directory(cache)?
        } else {
            RegistryIndex::default()
        };

        let url_index = if let Some(cache) = cache {
            UrlIndex::try_from_directory(cache)?
        } else {
            UrlIndex::default()
        };

        let mut local = vec![];
        let mut remote = vec![];
        let mut extraneous = vec![];

        for requirement in requirements {
            let package = PackageName::normalize(&requirement.name);

            // Filter out already-installed packages.
            if let Some(distribution) = site_packages.remove(&package) {
                if requirement.is_satisfied_by(distribution.version()) {
                    debug!("Requirement already satisfied: {distribution}",);
                    continue;
                }
                extraneous.push(distribution);
            }

            // Identify any locally-available distributions that satisfy the requirement.
            match requirement.version_or_url.as_ref() {
                None | Some(VersionOrUrl::VersionSpecifier(_)) => {
                    if let Some(distribution) = registry_index.get(&package).filter(|dist| {
                        let CachedDistribution::Registry(_name, version, _path) = dist else {
                            return false;
                        };
                        requirement.is_satisfied_by(version)
                    }) {
                        debug!("Requirement already cached: {distribution}");
                        local.push(distribution.clone());
                        continue;
                    }
                }
                Some(VersionOrUrl::Url(url)) => {
                    if let Some(distribution) = url_index.get(&package, url) {
                        debug!("Requirement already cached: {distribution}");
                        local.push(distribution.clone());
                        continue;
                    }
                }
            }

            debug!("Identified uncached requirement: {requirement}");
            remote.push(requirement.clone());
        }

        // Remove any unnecessary packages.
        for (_package, dist_info) in site_packages {
            debug!("Unnecessary package: {dist_info}");
            extraneous.push(dist_info);
        }

        Ok(PartitionedRequirements {
            local,
            remote,
            extraneous,
        })
    }
}