//! Plugin binary download, checksum-verification, and installation.
//!
//! # Security note — SSRF / RCE guard
//!
//! `fetch_manifest` only accepts a fixed caller-supplied trusted string. It
//! **must never** accept an arbitrary URL from an untrusted HTTP caller.
//! Letting an unauthenticated or insufficiently-privileged caller supply the
//! manifest URL would trivially enable SSRF (reaching internal services) or
//! RCE (installing an attacker-controlled binary). The HTTP handler is
//! responsible for supplying the fixed, compile-time constant URL.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};
use utoipa::ToSchema;

/// Per-platform download descriptor embedded in a manifest.
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct PlatformAsset {
    /// Direct download URL for the `.tar.gz` archive.
    pub url: String,
    /// Expected SHA-256 hex digest of the archive bytes.
    pub sha256: String,
}

/// Registry manifest for a single external plugin.
///
/// JSON shape:
/// ```json
/// {
///   "name": "vibetemps",
///   "version": "1.2.3",
///   "platforms": {
///     "linux-amd64":  { "url": "...", "sha256": "..." },
///     "linux-arm64":  { "url": "...", "sha256": "..." },
///     "darwin-amd64": { "url": "...", "sha256": "..." },
///     "darwin-arm64": { "url": "...", "sha256": "..." }
///   }
/// }
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct PluginRegistryManifest {
    /// Plugin name (e.g. `"vibetemps"`).
    pub name: String,
    /// Semver version string (e.g. `"1.2.3"`).
    pub version: String,
    /// Map from platform key (`"linux-amd64"`, etc.) to download descriptor.
    pub platforms: HashMap<String, PlatformAsset>,
}

/// Installer for external plugin binaries.
///
/// Stateless — all methods are `&self` or free functions; the struct is a
/// namespace for the install flow.
pub struct PluginInstaller;

impl PluginInstaller {
    pub fn new() -> Self {
        Self
    }

    /// Fetch and parse the registry manifest from a trusted, fixed URL.
    ///
    /// # SSRF guard
    ///
    /// The `manifest_url` parameter **must** be a compile-time constant
    /// supplied by the HTTP handler, not a value taken from an incoming
    /// request body. Validating the URL here (instead of in the handler)
    /// would be too late — the caller is responsible for this invariant.
    pub async fn fetch_manifest(manifest_url: &str) -> anyhow::Result<PluginRegistryManifest> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build HTTP client: {}", e))?;

        let response = client
            .get(manifest_url)
            .header("User-Agent", "temps-plugin-installer")
            .send()
            .await
            .map_err(|e| {
                anyhow::anyhow!("Failed to fetch manifest from {}: {}", manifest_url, e)
            })?;

        if !response.status().is_success() {
            return Err(anyhow::anyhow!(
                "Manifest fetch from {} returned HTTP {}",
                manifest_url,
                response.status()
            ));
        }

        response
            .json::<PluginRegistryManifest>()
            .await
            .map_err(|e| {
                anyhow::anyhow!("Failed to parse manifest JSON from {}: {}", manifest_url, e)
            })
    }

    /// Download, verify, extract, and atomically install a plugin binary.
    ///
    /// Steps performed in order:
    /// 1. Resolve the current OS/arch to a platform key.
    /// 2. Look up the platform asset in `manifest.platforms`.
    /// 3. Download the tarball.
    /// 4. Verify the SHA-256 checksum — aborts **before** touching disk on mismatch.
    /// 5. Extract the plugin binary from the tarball (entry named `binary_name`).
    /// 6. Write to `plugins_dir/<binary_name>` via temp-file → atomic rename.
    ///
    /// Returns the path of the installed binary on success.
    ///
    /// Note: this method is a pure filesystem operation. Calling
    /// `ExternalPluginManager::reload_plugin` after this returns is the
    /// caller's responsibility.
    pub async fn install(
        &self,
        binary_name: &str,
        manifest: &PluginRegistryManifest,
        plugins_dir: &Path,
    ) -> anyhow::Result<PathBuf> {
        // 1. Resolve platform key
        let platform_key = platform_target()
            .map_err(|e| anyhow::anyhow!("Cannot install plugin '{}': {}", manifest.name, e))?;

        // 2. Look up platform asset
        let asset = manifest.platforms.get(&platform_key).ok_or_else(|| {
            anyhow::anyhow!(
                "Plugin '{}' v{} has no release for platform '{}'. Available: {}",
                manifest.name,
                manifest.version,
                platform_key,
                manifest
                    .platforms
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;

        info!(
            plugin = %manifest.name,
            version = %manifest.version,
            platform = %platform_key,
            url = %asset.url,
            "Downloading plugin binary",
        );

        // 3. Download tarball
        let bytes = download_asset(&asset.url).await?;

        debug!(plugin = %manifest.name, bytes = bytes.len(), "Download complete, verifying checksum");

        // 4. Verify checksum BEFORE writing anything to disk.
        //    The checksum_text format expected by verify_checksum is
        //    "<hex>  <filename>" (sha256sum style). We pass the binary_name
        //    as the filename component — it is ignored by the parser, only the
        //    hash token matters.
        let checksum_text = format!("{}  {}", asset.sha256, binary_name);
        temps_core::checksum::verify_checksum(&bytes, &checksum_text).map_err(|e| {
            anyhow::anyhow!(
                "Checksum verification failed for plugin '{}' v{} (platform '{}'): {}",
                manifest.name,
                manifest.version,
                platform_key,
                e
            )
        })?;

        debug!(plugin = %manifest.name, "Checksum verified, extracting binary from tarball");

        // 5. Extract binary from tarball
        let binary_bytes = extract_binary_from_tarball(&bytes, binary_name).map_err(|e| {
            anyhow::anyhow!(
                "Failed to extract '{}' from plugin '{}' v{} tarball: {}",
                binary_name,
                manifest.name,
                manifest.version,
                e
            )
        })?;

        // Ensure the plugins directory exists
        tokio::fs::create_dir_all(plugins_dir).await.map_err(|e| {
            anyhow::anyhow!(
                "Failed to create plugins directory {}: {}",
                plugins_dir.display(),
                e
            )
        })?;

        let dest_path = plugins_dir.join(binary_name);

        // 6. Atomic temp-file → rename install
        write_binary_atomically(&dest_path, &binary_bytes).map_err(|e| {
            anyhow::anyhow!(
                "Failed to write plugin '{}' binary to {}: {}",
                manifest.name,
                dest_path.display(),
                e
            )
        })?;

        info!(
            plugin = %manifest.name,
            version = %manifest.version,
            path = %dest_path.display(),
            "Plugin binary installed successfully",
        );

        Ok(dest_path)
    }
}

impl Default for PluginInstaller {
    fn default() -> Self {
        Self::new()
    }
}

/// Map the current OS/architecture to a platform key used in plugin manifests.
///
/// Returns one of: `"linux-amd64"`, `"linux-arm64"`, `"darwin-amd64"`,
/// `"darwin-arm64"`. Returns an error on any unsupported platform.
pub fn platform_target() -> anyhow::Result<String> {
    use std::env::consts::{ARCH, OS};
    let key = match (OS, ARCH) {
        ("macos", "x86_64") => "darwin-amd64",
        ("macos", "aarch64") => "darwin-arm64",
        ("linux", "x86_64") => "linux-amd64",
        ("linux", "aarch64") => "linux-arm64",
        _ => {
            return Err(anyhow::anyhow!(
                "Unsupported platform: {} {}. Plugin install is available for: \
                 macOS (x86_64, aarch64), Linux (x86_64, aarch64)",
                OS,
                ARCH
            ));
        }
    };
    Ok(key.to_string())
}

/// Download a URL and return the raw bytes.
async fn download_asset(url: &str) -> anyhow::Result<Vec<u8>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build HTTP client: {}", e))?;

    let response = client
        .get(url)
        .header("User-Agent", "temps-plugin-installer")
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to download asset from {}: {}", url, e))?;

    if !response.status().is_success() {
        return Err(anyhow::anyhow!(
            "Download from {} returned HTTP {}",
            url,
            response.status()
        ));
    }

    response
        .bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| anyhow::anyhow!("Failed to read download body from {}: {}", url, e))
}

/// Extract a single named entry from a gzip-compressed tar archive.
///
/// `entry_name` is matched against the archive entry's file name (the last
/// path component), not the full path inside the archive.
fn extract_binary_from_tarball(tarball_bytes: &[u8], entry_name: &str) -> anyhow::Result<Vec<u8>> {
    let decoder = GzDecoder::new(tarball_bytes);
    let mut archive = tar::Archive::new(decoder);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;

        if path.file_name().map(|n| n == entry_name).unwrap_or(false) {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)?;
            return Ok(buf);
        }
    }

    Err(anyhow::anyhow!(
        "Entry '{}' not found in tarball",
        entry_name
    ))
}

/// Write `binary_bytes` to `dest` atomically by writing to a sibling temp
/// file first and then renaming. Sets the executable bit before the rename.
fn write_binary_atomically(dest: &Path, binary_bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write as _;

    let parent = dest.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "Destination path {} has no parent directory",
            dest.display()
        )
    })?;

    // Use a uniquely-named temp file in the same directory so the rename is
    // guaranteed to be on the same filesystem (required for atomicity).
    let mut tmp_file = tempfile::NamedTempFile::new_in(parent).map_err(|e| {
        anyhow::anyhow!("Failed to create temp file in {}: {}", parent.display(), e)
    })?;

    tmp_file
        .write_all(binary_bytes)
        .map_err(|e| anyhow::anyhow!("Failed to write binary data to temp file: {}", e))?;

    // Set executable bit before rename
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = tmp_file
            .as_file()
            .metadata()
            .map_err(|e| anyhow::anyhow!("Failed to read temp file metadata: {}", e))?
            .permissions();
        perms.set_mode(perms.mode() | 0o111);
        tmp_file
            .as_file()
            .set_permissions(perms)
            .map_err(|e| anyhow::anyhow!("Failed to set executable bit: {}", e))?;
    }

    // Atomic rename — replaces dest if it already exists
    tmp_file
        .persist(dest)
        .map_err(|e| anyhow::anyhow!("Failed to persist binary to {}: {}", dest.display(), e))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_target_returns_supported_key() {
        // Running on the CI host — whatever platform this is must be one we support.
        let result = platform_target();
        assert!(result.is_ok(), "platform_target() failed: {:?}", result);
        let key = result.unwrap();
        assert!(
            ["linux-amd64", "linux-arm64", "darwin-amd64", "darwin-arm64"].contains(&key.as_str()),
            "Unexpected platform key: {key}"
        );
    }

    #[test]
    fn manifest_deserializes_correctly() {
        let json = r#"{
            "name": "vibetemps",
            "version": "1.2.3",
            "platforms": {
                "linux-amd64": { "url": "https://example.com/v.tar.gz", "sha256": "abc123" },
                "darwin-arm64": { "url": "https://example.com/d.tar.gz", "sha256": "def456" }
            }
        }"#;
        let manifest: PluginRegistryManifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.name, "vibetemps");
        assert_eq!(manifest.version, "1.2.3");
        assert!(manifest.platforms.contains_key("linux-amd64"));
        assert_eq!(manifest.platforms["darwin-arm64"].sha256, "def456");
    }

    #[test]
    fn manifest_serializes_round_trip() {
        let manifest = PluginRegistryManifest {
            name: "myplugin".to_string(),
            version: "0.1.0".to_string(),
            platforms: {
                let mut m = HashMap::new();
                m.insert(
                    "linux-amd64".to_string(),
                    PlatformAsset {
                        url: "https://example.com/p.tar.gz".to_string(),
                        sha256: "deadbeef".to_string(),
                    },
                );
                m
            },
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let back: PluginRegistryManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, manifest.name);
        assert_eq!(back.version, manifest.version);
        assert_eq!(back.platforms["linux-amd64"].sha256, "deadbeef");
    }

    #[test]
    fn extract_binary_not_found_returns_error() {
        // Build a minimal valid gzip+tar with a single entry named "other"
        use flate2::write::GzEncoder;
        use flate2::Compression;

        let mut gz_buf = Vec::new();
        {
            let enc = GzEncoder::new(&mut gz_buf, Compression::default());
            let mut tar = tar::Builder::new(enc);
            let data = b"binary data here";
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, "other-binary", data.as_ref())
                .unwrap();
            tar.finish().unwrap();
        }

        let result = extract_binary_from_tarball(&gz_buf, "mybin");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("not found in tarball"));
    }

    #[test]
    fn extract_binary_found_returns_bytes() {
        use flate2::write::GzEncoder;
        use flate2::Compression;

        let expected = b"the real binary";
        let mut gz_buf = Vec::new();
        {
            let enc = GzEncoder::new(&mut gz_buf, Compression::default());
            let mut tar = tar::Builder::new(enc);
            let mut header = tar::Header::new_gnu();
            header.set_size(expected.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, "mybin", expected.as_ref())
                .unwrap();
            tar.finish().unwrap();
        }

        let result = extract_binary_from_tarball(&gz_buf, "mybin").unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn write_binary_atomically_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("my-plugin");
        write_binary_atomically(&dest, b"plugin-binary-bytes").unwrap();
        assert!(dest.exists());
        let contents = std::fs::read(&dest).unwrap();
        assert_eq!(contents, b"plugin-binary-bytes");
    }

    #[test]
    #[cfg(unix)]
    fn write_binary_atomically_sets_executable_bit() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("executable-plugin");
        write_binary_atomically(&dest, b"data").unwrap();
        let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
        assert!(
            mode & 0o111 != 0,
            "Executable bit should be set: mode={mode:#o}"
        );
    }

    #[tokio::test]
    async fn install_fails_on_unsupported_platform_in_manifest() {
        let manifest = PluginRegistryManifest {
            name: "testplugin".to_string(),
            version: "0.0.1".to_string(),
            platforms: HashMap::new(), // empty — no platform matches
        };
        let tmp = tempfile::tempdir().unwrap();
        let installer = PluginInstaller::new();
        let result = installer
            .install("testplugin-bin", &manifest, tmp.path())
            .await;
        assert!(result.is_err());
        // Error must mention the plugin name
        assert!(result.unwrap_err().to_string().contains("testplugin"));
    }
}
