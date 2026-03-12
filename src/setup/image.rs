//! Image catalog, download, and preparation.
//!
//! Resolves an `ImageSpec` (distro + version + arch) to download URLs,
//! fetches and caches the assets, and converts disk formats as needed.

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
    /// `None` for diskless distros (e.g., Alpine netboot with tmpfs rootfs).
    pub disk: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Catalog resolution
// ---------------------------------------------------------------------------

/// A single downloadable asset with its expected filename.
#[derive(Debug)]
struct ImageAsset {
    filename: &'static str,
    url: String,
    /// Expected SHA-256 hex digest for verification. `None` means log-only (no comparison).
    expected_sha256: Option<&'static str>,
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
        "alpine" => resolve_alpine(version, arch),
        _ => Err(SetupError::UnsupportedImage(format!(
            "unknown distro '{}'. Supported: ubuntu, alpine. \
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
            url: format!("{unpacked}/ubuntu-{version}-server-cloudimg-{arch_str}-vmlinuz-generic"),
            expected_sha256: None,
        },
        ImageAsset {
            filename: "initramfs",
            url: format!("{unpacked}/ubuntu-{version}-server-cloudimg-{arch_str}-initrd-generic"),
            expected_sha256: None,
        },
        ImageAsset {
            filename: "disk.img",
            url: format!("{base}/ubuntu-{version}-server-cloudimg-{arch_str}.img"),
            expected_sha256: None,
        },
    ])
}

fn resolve_alpine(version: &str, arch: Arch) -> Result<Vec<ImageAsset>, SetupError> {
    let arch_str = match arch {
        Arch::Aarch64 => "aarch64",
        Arch::X86_64 => "x86_64",
    };

    let base = format!("https://dl-cdn.alpinelinux.org/alpine/v{version}/releases/{arch_str}");

    // Alpine only provides kernel + initramfs for netboot.
    // The .iso is NOT a root disk image — it cannot be attached as /dev/vda1
    // or converted with qemu-img. Alpine boots diskless with tmpfs rootfs.
    Ok(vec![
        ImageAsset {
            filename: "vmlinuz",
            url: format!("{base}/netboot/vmlinuz-virt"),
            expected_sha256: None,
        },
        ImageAsset {
            filename: "initramfs",
            url: format!("{base}/netboot/initramfs-virt"),
            expected_sha256: None,
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

    for asset in &assets {
        let path = image_dir.join(asset.filename);
        if path.exists() {
            tracing::debug!(file = %asset.filename, "cached, skipping download");
            continue;
        }
        tracing::info!(file = %asset.filename, url = %asset.url, "downloading");
        download_file(&asset.url, &path).await?;
        verify_download(&path, asset.filename, asset.expected_sha256)?;
    }

    let kernel = image_dir.join("vmlinuz");
    let initramfs_path = image_dir.join("initramfs");
    let initramfs = if initramfs_path.exists() {
        Some(initramfs_path)
    } else {
        None
    };

    if !kernel.exists() {
        return Err(SetupError::AssetDownload(format!(
            "kernel not found after download: {}",
            kernel.display()
        )));
    }

    // On macOS, Apple VZ needs raw disk images — convert from QCOW2 if needed.
    // Disk is optional: diskless distros (e.g., Alpine netboot) have no disk asset.
    let disk_downloaded = image_dir.join("disk.img");
    let disk = if cfg!(target_os = "macos") {
        let raw_path = image_dir.join("disk.raw");
        if disk_downloaded.exists() && !raw_path.exists() {
            convert_to_raw(&disk_downloaded, &raw_path)?;
        }
        if raw_path.exists() {
            Some(raw_path)
        } else if disk_downloaded.exists() {
            Some(disk_downloaded)
        } else {
            None
        }
    } else if disk_downloaded.exists() {
        Some(disk_downloaded)
    } else {
        None
    };

    Ok(PreparedImage {
        kernel,
        initramfs,
        disk,
    })
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Verify a downloaded file: non-empty check + SHA-256 digest verification.
///
/// Rejects zero-byte downloads (indicates server error or truncated transfer).
/// When `expected_sha256` is `Some`, the computed digest is compared and an error
/// is returned on mismatch. When `None`, the digest is logged for manual auditing only.
fn verify_download(
    path: &Path,
    filename: &str,
    expected_sha256: Option<&str>,
) -> Result<(), SetupError> {
    let metadata = std::fs::metadata(path).map_err(SetupError::Io)?;
    if metadata.len() == 0 {
        // Remove the empty file so it isn't treated as cached on retry
        std::fs::remove_file(path).map_err(|e| {
            tracing::error!(path = %path.display(), "failed to remove empty download: {e}");
            SetupError::Io(e)
        })?;
        return Err(SetupError::AssetDownload(format!(
            "downloaded file is empty (0 bytes): {}",
            filename
        )));
    }

    // Compute SHA-256 for integrity verification.
    // Reads in 64KB chunks to avoid buffering large images.
    let mut file = std::fs::File::open(path).map_err(SetupError::Io)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        use std::io::Read;
        let n = file.read(&mut buf).map_err(SetupError::Io)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = format!("{:x}", hasher.finalize());

    if let Some(expected) = expected_sha256 {
        if digest != expected {
            return Err(SetupError::AssetDownload(format!(
                "SHA-256 mismatch for {}: expected {}, got {}",
                filename, expected, digest
            )));
        }
    }

    tracing::info!(
        file = filename,
        bytes = metadata.len(),
        sha256 = %digest,
        "download verified"
    );

    Ok(())
}

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

async fn download_file(url: &str, path: &Path) -> Result<(), SetupError> {
    // Write to a sibling temp file so interrupted downloads cannot poison the cache.
    // The final atomic rename only happens after fsync succeeds.
    let filename = path
        .file_name()
        .map(|n| format!(".{}.tmp", n.to_string_lossy()))
        .unwrap_or_else(|| ".download.tmp".into());
    let tmp_path = path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(&filename);

    let result = download_file_to_tmp(url, path, &tmp_path).await;
    if result.is_err() && tmp_path.exists() {
        if let Err(e) = std::fs::remove_file(&tmp_path) {
            tracing::warn!(path = %tmp_path.display(), "failed to clean up temp download: {e}");
        }
    }
    result
}

async fn download_file_to_tmp(
    url: &str,
    final_path: &Path,
    tmp_path: &Path,
) -> Result<(), SetupError> {
    use tokio::io::AsyncWriteExt;

    let client = reqwest::Client::new();
    let resp = client.get(url).send().await.map_err(|e| {
        SetupError::AssetDownload(format!("HTTP request failed for {}: {}", url, e))
    })?;

    if !resp.status().is_success() {
        return Err(SetupError::AssetDownload(format!(
            "HTTP {} for {}",
            resp.status(),
            url
        )));
    }

    // Stream to disk instead of buffering the whole response in memory.
    // VM images can be hundreds of MB.
    let mut file = tokio::fs::File::create(tmp_path)
        .await
        .map_err(SetupError::Io)?;
    let mut stream = resp.bytes_stream();
    let mut total_bytes = 0u64;

    use futures_util::StreamExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| {
            SetupError::AssetDownload(format!("failed to read response body from {}: {}", url, e))
        })?;
        file.write_all(&chunk).await.map_err(SetupError::Io)?;
        total_bytes += chunk.len() as u64;
    }
    file.flush().await.map_err(SetupError::Io)?;
    // fsync so the data is durable before we expose it via rename
    file.sync_all().await.map_err(SetupError::Io)?;
    drop(file);

    // Atomic rename: the final path is either intact or absent — never partial.
    std::fs::rename(tmp_path, final_path).map_err(SetupError::Io)?;

    tracing::info!(
        path = %final_path.display(),
        bytes = total_bytes,
        "downloaded"
    );
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
        // Must be one of the two variants
        assert!(matches!(arch, Arch::Aarch64 | Arch::X86_64));
    }

    #[test]
    fn resolve_ubuntu_returns_3_assets() {
        let spec = ImageSpec {
            distro: "ubuntu".into(),
            version: "24.04".into(),
            arch: Some(Arch::Aarch64),
        };
        let assets = resolve_image(&spec).unwrap();
        assert_eq!(assets.len(), 3);
        assert_eq!(assets[0].filename, "vmlinuz");
        assert_eq!(assets[1].filename, "initramfs");
        assert_eq!(assets[2].filename, "disk.img");
        assert!(assets[0].url.contains("arm64"));
    }

    #[test]
    fn resolve_alpine_returns_2_assets() {
        let spec = ImageSpec {
            distro: "alpine".into(),
            version: "3.20".into(),
            arch: Some(Arch::X86_64),
        };
        let assets = resolve_image(&spec).unwrap();
        // Alpine netboot: kernel + initramfs only (no disk image)
        assert_eq!(assets.len(), 2);
        assert_eq!(assets[0].filename, "vmlinuz");
        assert_eq!(assets[1].filename, "initramfs");
        assert!(assets[0].url.contains("x86_64"));
    }

    #[test]
    fn resolve_unknown_distro_fails() {
        let spec = ImageSpec {
            distro: "fedora".into(),
            version: "40".into(),
            arch: None,
        };
        let result = resolve_image(&spec);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("fedora"));
    }

    #[test]
    fn resolve_case_insensitive() {
        let spec = ImageSpec {
            distro: "Ubuntu".into(),
            version: "24.04".into(),
            arch: Some(Arch::X86_64),
        };
        let assets = resolve_image(&spec).unwrap();
        assert!(assets[0].url.contains("amd64"));
    }
}
