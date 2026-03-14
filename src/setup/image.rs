//! Image catalog, download, and preparation.
//!
//! Resolves an `ImageSpec` (distro + version + arch) to download URLs,
//! fetches and caches the assets, and converts disk formats as needed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::SetupError;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// CPU architecture for VM images.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    Aarch64,
    X86_64,
}

impl Arch {
    /// Detect the host architecture.
    pub fn host() -> Self {
        if cfg!(target_arch = "aarch64") {
            Arch::Aarch64
        } else {
            Arch::X86_64
        }
    }
}

impl std::fmt::Display for Arch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Arch::Aarch64 => write!(f, "aarch64"),
            Arch::X86_64 => write!(f, "x86_64"),
        }
    }
}

/// What OS image to download and prepare.
#[derive(Debug, Clone)]
pub struct ImageSpec {
    /// Distribution (e.g., "ubuntu", "alpine").
    pub distro: String,
    /// Version (e.g., "24.04", "3.20").
    pub version: String,
    /// Target architecture. Defaults to host arch if None.
    pub arch: Option<Arch>,
}

/// A prepared image — all files needed to boot a VM.
#[derive(Debug, Clone)]
pub struct PreparedImage {
    /// Path to the kernel image.
    pub kernel: PathBuf,
    /// Path to the initramfs (if the distro provides one).
    pub initramfs: Option<PathBuf>,
    /// Path to the root disk image (raw format, ready for CoW cloning).
    pub disk: PathBuf,
}

// ---------------------------------------------------------------------------
// Catalog resolution
// ---------------------------------------------------------------------------

/// A single downloadable asset with its expected filename.
#[derive(Debug)]
struct ImageAsset {
    filename: &'static str,
    url: String,
    source_filename: String,
    checksum_url: Option<String>,
}

/// Resolve an image spec to download URLs.
///
/// Returns an error if the distro/version/arch combination is not in the catalog.
/// Users can bring their own images instead.
fn resolve_image(spec: &ImageSpec) -> Result<Vec<ImageAsset>, SetupError> {
    let arch = spec.arch.unwrap_or_else(Arch::host);
    let distro = spec.distro.to_lowercase();
    let version = &spec.version;

    match distro.as_str() {
        "ubuntu" => resolve_ubuntu(version, arch),
        "alpine" => Err(SetupError::UnsupportedImage(
            "Alpine cloud-init images are not currently supported: the published assets here are \
             netboot kernel/initramfs plus an ISO, not a writable root disk. Provide your own \
             root disk image or use Ubuntu."
                .into(),
        )),
        _ => Err(SetupError::UnsupportedImage(format!(
            "unknown distro '{}'. Supported: ubuntu. \
             Or bring your own kernel + disk image.",
            distro
        ))),
    }
}

fn resolve_ubuntu(version: &str, arch: Arch) -> Result<Vec<ImageAsset>, SetupError> {
    let arch_str = match arch {
        Arch::Aarch64 => "arm64",
        Arch::X86_64 => "amd64",
    };

    let base = format!("https://cloud-images.ubuntu.com/releases/{version}/release");
    let unpacked = format!("{base}/unpacked");

    Ok(vec![
        ImageAsset {
            filename: "vmlinuz",
            source_filename: format!(
                "ubuntu-{version}-server-cloudimg-{arch_str}-vmlinuz-generic"
            ),
            url: format!(
                "{unpacked}/ubuntu-{version}-server-cloudimg-{arch_str}-vmlinuz-generic"
            ),
            checksum_url: Some(format!("{unpacked}/SHA256SUMS")),
        },
        ImageAsset {
            filename: "initramfs",
            source_filename: format!(
                "ubuntu-{version}-server-cloudimg-{arch_str}-initrd-generic"
            ),
            url: format!(
                "{unpacked}/ubuntu-{version}-server-cloudimg-{arch_str}-initrd-generic"
            ),
            checksum_url: Some(format!("{unpacked}/SHA256SUMS")),
        },
        ImageAsset {
            filename: "disk.img",
            source_filename: format!("ubuntu-{version}-server-cloudimg-{arch_str}.img"),
            url: format!("{base}/ubuntu-{version}-server-cloudimg-{arch_str}.img"),
            checksum_url: Some(format!("{base}/SHA256SUMS")),
        },
    ])
}

// ---------------------------------------------------------------------------
// Image preparation — download, cache, convert
// ---------------------------------------------------------------------------

/// Download and prepare a VM image. Idempotent — skips cached files.
///
/// Returns paths to the kernel, initramfs, and disk image.
///
/// The disk image is converted to raw format on macOS (Apple VZ requires raw).
/// On Linux, QCOW2 is kept as-is (Cloud Hypervisor supports it).
pub async fn prepare_image(
    spec: &ImageSpec,
    cache_dir: &Path,
) -> Result<PreparedImage, SetupError> {
    let arch = spec.arch.unwrap_or_else(Arch::host);
    let image_dir = cache_dir
        .join("images")
        .join(&spec.distro)
        .join(&spec.version)
        .join(arch.to_string());

    std::fs::create_dir_all(&image_dir).map_err(SetupError::Io)?;

    let assets = resolve_image(spec)?;
    let client = reqwest::Client::new();
    let mut checksum_cache: HashMap<String, HashMap<String, String>> = HashMap::new();

    for asset in &assets {
        let path = image_dir.join(asset.filename);
        let expected_sha256 = match asset.checksum_url.as_deref() {
            Some(checksum_url) => Some(
                expected_sha256(&client, checksum_url, &asset.source_filename, &mut checksum_cache)
                    .await?,
            ),
            None => None,
        };

        if path.exists() && verify_download(&path, expected_sha256.as_deref())? {
            tracing::debug!(file = %asset.filename, "cached and verified, skipping download");
            continue;
        }

        if path.exists() {
            tracing::warn!(
                file = %asset.filename,
                path = %path.display(),
                "cached asset failed verification; re-downloading"
            );
        }
        tracing::info!(file = %asset.filename, url = %asset.url, "downloading");
        download_file(&client, &asset.url, &path, expected_sha256.as_deref()).await?;
    }

    let kernel = image_dir.join("vmlinuz");
    let initramfs_path = image_dir.join("initramfs");
    let initramfs = if initramfs_path.exists() {
        Some(initramfs_path)
    } else {
        None
    };

    // On macOS, Apple VZ needs raw disk images — convert from QCOW2 if needed
    let disk_downloaded = image_dir.join("disk.img");
    let disk = if cfg!(target_os = "macos") {
        let raw_path = image_dir.join("disk.raw");
        if disk_downloaded.exists() && !raw_path.exists() {
            convert_to_raw(&disk_downloaded, &raw_path)?;
        }
        if raw_path.exists() {
            raw_path
        } else {
            disk_downloaded
        }
    } else {
        disk_downloaded
    };

    if !kernel.exists() {
        return Err(SetupError::AssetDownload(format!(
            "kernel not found after download: {}",
            kernel.display()
        )));
    }
    if !disk.exists() {
        return Err(SetupError::AssetDownload(format!(
            "disk image not found after download: {}",
            disk.display()
        )));
    }

    Ok(PreparedImage {
        kernel,
        initramfs,
        disk,
    })
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Convert a QCOW2 disk image to raw format.
fn convert_to_raw(qcow2: &Path, raw: &Path) -> Result<(), SetupError> {
    tracing::info!(
        src = %qcow2.display(),
        dst = %raw.display(),
        "converting disk image to raw format"
    );
    let output = std::process::Command::new("qemu-img")
        .args(["convert", "-f", "qcow2", "-O", "raw"])
        .arg(qcow2)
        .arg(raw)
        .output()
        .map_err(SetupError::Io)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SetupError::AssetDownload(format!(
            "qemu-img convert failed (exit {}): {}. \
             Install qemu-img: brew install qemu (macOS) or apt install qemu-utils (Linux)",
            output.status,
            stderr.trim()
        )));
    }
    Ok(())
}

async fn download_file(
    client: &reqwest::Client,
    url: &str,
    path: &Path,
    expected_sha256: Option<&str>,
) -> Result<(), SetupError> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| {
            SetupError::AssetDownload(format!("HTTP request failed for {}: {}", url, e))
        })?;

    if !resp.status().is_success() {
        return Err(SetupError::AssetDownload(format!(
            "HTTP {} for {}",
            resp.status(),
            url
        )));
    }

    let bytes = resp.bytes().await.map_err(|e| {
        SetupError::AssetDownload(format!(
            "failed to read response body from {}: {}",
            url, e
        ))
    })?;

    if let Some(expected) = expected_sha256 {
        verify_bytes(&bytes, expected, url)?;
    }

    std::fs::write(path, &bytes).map_err(SetupError::Io)?;
    tracing::info!(
        path = %path.display(),
        bytes = bytes.len(),
        "downloaded"
    );
    Ok(())
}

async fn expected_sha256(
    client: &reqwest::Client,
    checksum_url: &str,
    filename: &str,
    cache: &mut HashMap<String, HashMap<String, String>>,
) -> Result<String, SetupError> {
    if !cache.contains_key(checksum_url) {
        let manifest = fetch_checksum_manifest(client, checksum_url).await?;
        cache.insert(checksum_url.to_string(), manifest);
    }

    cache
        .get(checksum_url)
        .and_then(|manifest| manifest.get(filename))
        .cloned()
        .ok_or_else(|| {
            SetupError::AssetDownload(format!(
                "checksum manifest {} does not contain {}",
                checksum_url, filename
            ))
        })
}

async fn fetch_checksum_manifest(
    client: &reqwest::Client,
    checksum_url: &str,
) -> Result<HashMap<String, String>, SetupError> {
    let resp = client
        .get(checksum_url)
        .send()
        .await
        .map_err(|e| {
            SetupError::AssetDownload(format!(
                "failed to fetch checksum manifest {}: {}",
                checksum_url, e
            ))
        })?;

    if !resp.status().is_success() {
        return Err(SetupError::AssetDownload(format!(
            "HTTP {} for checksum manifest {}",
            resp.status(),
            checksum_url
        )));
    }

    let body = resp.text().await.map_err(|e| {
        SetupError::AssetDownload(format!(
            "failed to read checksum manifest {}: {}",
            checksum_url, e
        ))
    })?;

    parse_checksum_manifest(&body)
}

fn parse_checksum_manifest(body: &str) -> Result<HashMap<String, String>, SetupError> {
    let mut manifest = HashMap::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(split_at) = line.find(char::is_whitespace) else {
            return Err(SetupError::AssetDownload(format!(
                "malformed checksum line: {}",
                line
            )));
        };
        let (digest, path) = line.split_at(split_at);
        let digest = digest.trim();
        let filename = path
            .trim()
            .trim_start_matches('*')
            .trim_start_matches("./");
        if digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit()) {
            manifest.insert(filename.to_string(), digest.to_ascii_lowercase());
        }
    }

    if manifest.is_empty() {
        return Err(SetupError::AssetDownload(
            "checksum manifest did not contain any SHA256 entries".into(),
        ));
    }

    Ok(manifest)
}

fn verify_download(path: &Path, expected_sha256: Option<&str>) -> Result<bool, SetupError> {
    let Some(expected_sha256) = expected_sha256 else {
        return Ok(true);
    };
    let bytes = std::fs::read(path).map_err(SetupError::Io)?;
    verify_bytes(&bytes, expected_sha256, &path.display().to_string())?;
    Ok(true)
}

fn verify_bytes(bytes: &[u8], expected_sha256: &str, label: &str) -> Result<(), SetupError> {
    let actual_sha256 = format!("{:x}", Sha256::digest(bytes));
    if actual_sha256 != expected_sha256 {
        return Err(SetupError::AssetDownload(format!(
            "SHA256 mismatch for {}: expected {}, got {}",
            label, expected_sha256, actual_sha256
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arch_display() {
        assert_eq!(Arch::Aarch64.to_string(), "aarch64");
        assert_eq!(Arch::X86_64.to_string(), "x86_64");
    }

    #[test]
    fn arch_host_returns_valid() {
        let arch = Arch::host();
        assert!(matches!(arch, Arch::Aarch64 | Arch::X86_64));
    }

    #[test]
    fn resolve_ubuntu_returns_3_assets() {
        let spec = ImageSpec {
            distro: "ubuntu".into(),
            version: "24.04".into(),
            arch: Some(Arch::Aarch64),
        };
        let assets = resolve_image(&spec).expect("ubuntu assets");
        assert_eq!(assets.len(), 3);
        assert_eq!(assets[0].filename, "vmlinuz");
        assert_eq!(assets[1].filename, "initramfs");
        assert_eq!(assets[2].filename, "disk.img");
        assert!(assets[0].url.contains("arm64"));
    }

    #[test]
    fn resolve_alpine_is_explicitly_unsupported() {
        let spec = ImageSpec {
            distro: "alpine".into(),
            version: "3.20".into(),
            arch: Some(Arch::X86_64),
        };
        let err = resolve_image(&spec).expect_err("alpine should fail fast");
        assert!(err.to_string().contains("not currently supported"));
    }

    #[test]
    fn resolve_unknown_distro_fails() {
        let spec = ImageSpec {
            distro: "fedora".into(),
            version: "40".into(),
            arch: None,
        };
        let err = resolve_image(&spec)
            .expect_err("unknown distro should fail")
            .to_string();
        assert!(err.contains("fedora"));
    }

    #[test]
    fn resolve_case_insensitive() {
        let spec = ImageSpec {
            distro: "Ubuntu".into(),
            version: "24.04".into(),
            arch: Some(Arch::X86_64),
        };
        let assets = resolve_image(&spec).expect("ubuntu assets");
        assert!(assets[0].url.contains("amd64"));
    }

    #[test]
    fn parse_checksum_manifest_supports_coreutils_format() {
        let manifest = parse_checksum_manifest(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef *disk.img\n\
             abcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd ./initramfs\n",
        )
        .expect("manifest");

        assert_eq!(
            manifest.get("disk.img"),
            Some(&"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string())
        );
        assert_eq!(
            manifest.get("initramfs"),
            Some(&"abcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd".to_string())
        );
    }

    #[test]
    fn parse_checksum_manifest_rejects_malformed_lines() {
        let err = parse_checksum_manifest("not-a-valid-line")
            .expect_err("malformed manifest should fail")
            .to_string();
        assert!(err.contains("malformed checksum line"));
    }

    #[test]
    fn verify_bytes_rejects_digest_mismatch() {
        let err = verify_bytes(b"vm-rs", &"00".repeat(32), "fixture")
            .expect_err("mismatched digest should fail")
            .to_string();
        assert!(err.contains("SHA256 mismatch"));
    }
}
