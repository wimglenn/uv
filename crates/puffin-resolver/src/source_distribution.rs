use std::path::PathBuf;
use std::str::FromStr;

use anyhow::Result;
use fs_err::tokio as fs;
use tempfile::tempdir;
use tokio_util::compat::FuturesAsyncReadCompatExt;
use tracing::debug;
use zip::ZipArchive;

use distribution_filename::WheelFilename;
use platform_tags::Tags;
use puffin_client::RegistryClient;
use puffin_distribution::RemoteDistributionRef;
use puffin_package::pypi_types::Metadata21;
use puffin_traits::BuildContext;

const BUILT_WHEELS_CACHE: &str = "built-wheels-v0";

const REMOTE_WHEELS_CACHE: &str = "remote-wheels-v0";

/// Stores wheels built from source distributions. We need to keep those separate from the regular
/// wheel cache since a wheel with the same name may be uploaded after we made our build and in that
/// case the hashes would clash.
pub(crate) struct SourceDistributionBuildTree<'a, T: BuildContext>(&'a T);

impl<'a, T: BuildContext> SourceDistributionBuildTree<'a, T> {
    /// Initialize a [`SourceDistributionBuildTree`] from a [`BuildContext`].
    pub(crate) fn new(build_context: &'a T) -> Self {
        Self(build_context)
    }

    /// Read the [`Metadata21`] from a built source distribution, if it exists in the cache.
    pub(crate) fn find_dist_info(
        &self,
        distribution: &RemoteDistributionRef<'_>,
        tags: &Tags,
    ) -> Result<Option<Metadata21>> {
        self.find_wheel(distribution, tags)
            .as_ref()
            .map(read_dist_info)
            .transpose()
    }

    /// Download and build a source distribution, storing the built wheel in the cache.
    pub(crate) async fn download_and_build_sdist(
        &self,
        distribution: &RemoteDistributionRef<'_>,
        client: &RegistryClient,
    ) -> Result<Metadata21> {
        debug!("Building: {distribution}");
        let url = distribution.url()?;
        let reader = client.stream_external(&url).await?;
        let mut reader = tokio::io::BufReader::new(reader.compat());
        let temp_dir = tempdir()?;

        // Download the source distribution.
        let sdist_filename = distribution.filename()?;
        let sdist_file = temp_dir.path().join(sdist_filename.as_ref());
        let mut writer = tokio::fs::File::create(&sdist_file).await?;
        tokio::io::copy(&mut reader, &mut writer).await?;

        // Create a directory for the wheel.
        let wheel_dir = self.0.cache().map_or_else(
            || temp_dir.path().join(BUILT_WHEELS_CACHE),
            |cache| cache.join(BUILT_WHEELS_CACHE).join(distribution.id()),
        );
        fs::create_dir_all(&wheel_dir).await?;

        // Build the wheel.
        let disk_filename = self
            .0
            .build_source_distribution(&sdist_file, &wheel_dir)
            .await?;

        // Read the metadata from the wheel.
        let wheel = CachedWheel {
            path: wheel_dir.join(&disk_filename),
            filename: WheelFilename::from_str(&disk_filename)?,
        };
        let metadata21 = read_dist_info(&wheel)?;

        debug!("Finished building: {distribution}");
        Ok(metadata21)
    }

    pub(crate) async fn download_wheel(
        &self,
        distribution: &RemoteDistributionRef<'_>,
        client: &RegistryClient,
    ) -> Result<Metadata21> {
        debug!("Downloading: {distribution}");
        let url = distribution.url()?;
        let reader = client.stream_external(&url).await?;
        let mut reader = tokio::io::BufReader::new(reader.compat());
        let temp_dir = tempdir()?;

        // Create a directory for the wheel.
        let wheel_dir = self.0.cache().map_or_else(
            || temp_dir.path().join(REMOTE_WHEELS_CACHE),
            |cache| cache.join(REMOTE_WHEELS_CACHE).join(distribution.id()),
        );
        fs::create_dir_all(&wheel_dir).await?;

        // Download the wheel.
        let wheel_filename = distribution.filename()?;
        let wheel_file = wheel_dir.join(wheel_filename.as_ref());
        let mut writer = tokio::fs::File::create(&wheel_file).await?;
        tokio::io::copy(&mut reader, &mut writer).await?;

        // Read the metadata from the wheel.
        let wheel = CachedWheel {
            path: wheel_file,
            filename: WheelFilename::from_str(&wheel_filename)?,
        };
        let metadata21 = read_dist_info(&wheel)?;

        debug!("Finished downloading: {distribution}");
        Ok(metadata21)
    }

    /// Search for a wheel matching the tags that was built from the given source distribution.
    fn find_wheel(
        &self,
        distribution: &RemoteDistributionRef<'_>,
        tags: &Tags,
    ) -> Option<CachedWheel> {
        let wheel_dir = self
            .0
            .cache()?
            .join(BUILT_WHEELS_CACHE)
            .join(distribution.id());
        let Ok(read_dir) = fs_err::read_dir(wheel_dir) else {
            return None;
        };
        for entry in read_dir {
            let Ok(entry) = entry else {
                continue;
            };
            let Ok(filename) =
                WheelFilename::from_str(entry.file_name().to_string_lossy().as_ref())
            else {
                continue;
            };
            if filename.is_compatible(tags) {
                let path = entry.path().clone();
                return Some(CachedWheel { path, filename });
            }
        }
        None
    }
}

#[derive(Debug)]
struct CachedWheel {
    path: PathBuf,
    filename: WheelFilename,
}

/// Read the [`Metadata21`] from a wheel.
fn read_dist_info(wheel: &CachedWheel) -> Result<Metadata21> {
    let mut archive = ZipArchive::new(fs_err::File::open(&wheel.path)?)?;
    let dist_info_prefix = install_wheel_rs::find_dist_info(&wheel.filename, &mut archive)?;
    let dist_info = std::io::read_to_string(
        archive.by_name(&format!("{dist_info_prefix}.dist-info/METADATA"))?,
    )?;
    Ok(Metadata21::parse(dist_info.as_bytes())?)
}