//! Content-addressable OCI image store.
//!
//! Layout:
//!   <root>/images/
//!     blobs/sha256/<hash>          -- raw layer tarballs + config JSON
//!     manifests/<image>/<tag>.json -- cached manifests
//!
//! Same image used by multiple services = stored once (dedup by digest).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::registry::OciError;

// ---------------------------------------------------------------------------
// Typed OCI JSON structures (replacing serde_json::Value)
// ---------------------------------------------------------------------------

/// Raw OCI / Docker manifest as it appears on the wire.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawManifest {
    media_type: Option<String>,
    schema_version: Option<u64>,
    config: Option<RawDescriptor>,
    layers: Option<Vec<RawDescriptor>>,
}

/// OCI content descriptor (used for config and layer references).
#[derive(Deserialize)]
struct RawDescriptor {
    digest: Option<String>,
}

/// Raw OCI image configuration JSON.
#[derive(Deserialize)]
struct RawImageRoot {
    config: Option<RawContainerConfig>,
}

/// Container config section within the image config.
#[derive(Deserialize)]
#[allow(non_snake_case)]
struct RawContainerConfig {
    Entrypoint: Option<Vec<String>>,
    Cmd: Option<Vec<String>>,
    Env: Option<Vec<String>>,
    WorkingDir: Option<String>,
    User: Option<String>,
    ExposedPorts: Option<HashMap<String, serde_json::Value>>,
}

/// Local OCI blob store.
pub struct ImageStore {
    root: PathBuf,
}

/// Parsed OCI image manifest.
#[derive(Debug, Clone)]
pub struct ImageManifest {
    pub config_digest: String,
    pub layer_digests: Vec<String>,
    pub media_type: String,
}

/// Parsed OCI image config (the parts needed to run the process).
#[derive(Debug, Clone, Default)]
pub struct ImageConfig {
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    pub env: Vec<String>,
    pub working_dir: String,
    pub user: String,
    pub exposed_ports: Vec<u16>,
}

impl ImageStore {
    pub fn new(data_dir: &Path) -> Result<Self, OciError> {
        let root = data_dir.join("images");
        std::fs::create_dir_all(root.join("blobs/sha256"))?;
        std::fs::create_dir_all(root.join("manifests"))?;
        Ok(ImageStore { root })
    }

    /// Check if a blob exists locally.
    pub fn has_blob(&self, digest: &str) -> bool {
        self.blob_path(digest).exists()
    }

    /// Get the path to a blob by its sha256 digest.
    pub fn blob_path(&self, digest: &str) -> PathBuf {
        let hash = digest.strip_prefix("sha256:").unwrap_or(digest);
        self.root.join("blobs/sha256").join(hash)
    }

    /// Write a blob and verify its digest.
    pub fn put_blob(&self, digest: &str, data: &[u8]) -> Result<PathBuf, OciError> {
        let expected_hash = digest.strip_prefix("sha256:").unwrap_or(digest);

        let mut hasher = Sha256::new();
        hasher.update(data);
        let actual_hash = format!("{:x}", hasher.finalize());
        if actual_hash != expected_hash {
            return Err(OciError::Blob(format!(
                "digest mismatch: expected sha256:{}, got sha256:{}",
                expected_hash, actual_hash
            )));
        }

        let path = self.blob_path(digest);
        std::fs::write(&path, data)?;
        Ok(path)
    }

    /// Read a blob's bytes.
    pub fn get_blob(&self, digest: &str) -> Result<Vec<u8>, OciError> {
        let path = self.blob_path(digest);
        std::fs::read(&path).map_err(|e| OciError::Blob(format!("failed to read blob {}: {}", digest, e)))
    }

    /// Save a manifest for an image reference.
    pub fn put_manifest(&self, image: &str, tag: &str, data: &[u8]) -> Result<(), OciError> {
        let dir = self.root.join("manifests").join(sanitize_name(image));
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join(format!("{}.json", sanitize_name(tag))), data)?;
        Ok(())
    }

    /// Load a cached manifest. Returns `Ok(None)` if not cached.
    pub fn get_manifest(&self, image: &str, tag: &str) -> Result<Option<Vec<u8>>, OciError> {
        let path = self
            .root
            .join("manifests")
            .join(sanitize_name(image))
            .join(format!("{}.json", sanitize_name(tag)));
        match std::fs::read(&path) {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(OciError::Io(e)),
        }
    }

    /// List all cached images as (repository, tag, layers, size_bytes) tuples.
    pub fn list_images(&self) -> Result<Vec<(String, String, usize, u64)>, OciError> {
        let manifests_dir = self.root.join("manifests");
        let mut results = Vec::new();

        let repos = std::fs::read_dir(&manifests_dir)?;

        for repo_entry in repos {
            let repo_entry = repo_entry?;
            if !repo_entry.file_type()?.is_dir() {
                continue;
            }
            let repo_name = unsanitize_name(&repo_entry.file_name().to_string_lossy());
            let tags = std::fs::read_dir(repo_entry.path())?;

            for tag_entry in tags {
                let tag_entry = tag_entry?;
                let filename = tag_entry.file_name().to_string_lossy().to_string();
                if !filename.ends_with(".json") {
                    continue;
                }
                let tag = unsanitize_name(&filename[..filename.len() - 5]);

                let data = std::fs::read(tag_entry.path())?;
                let manifest = Self::parse_manifest(&data)?;
                let size: u64 = manifest
                    .layer_digests
                    .iter()
                    .map(|d| {
                        let path = self.blob_path(d);
                        std::fs::metadata(&path).map(|m| m.len())
                    })
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .sum();
                let layers = manifest.layer_digests.len();

                results.push((repo_name.clone(), tag, layers, size));
            }
        }

        results.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        Ok(results)
    }

    /// Parse an OCI/Docker manifest JSON.
    pub fn parse_manifest(data: &[u8]) -> Result<ImageManifest, OciError> {
        let raw: RawManifest = serde_json::from_slice(data)
            .map_err(|e| OciError::ManifestParse(format!("invalid JSON: {}", e)))?;

        let media_type = raw.media_type.as_deref()
            .or_else(|| raw.schema_version.map(|_| ""))
            .unwrap_or("");

        if media_type.contains("manifest.list") || media_type.contains("index") {
            return Err(OciError::ManifestList);
        }

        let config_digest = raw.config
            .and_then(|c| c.digest)
            .ok_or_else(|| OciError::ManifestParse("missing config digest".into()))?;

        let layers = raw.layers
            .ok_or_else(|| OciError::ManifestParse("missing layers".into()))?;
        let layer_digests: Vec<String> = layers
            .into_iter()
            .map(|l| {
                l.digest
                    .ok_or_else(|| OciError::ManifestParse("missing layer digest".into()))
            })
            .collect::<Result<_, _>>()?;

        Ok(ImageManifest {
            config_digest,
            layer_digests,
            media_type: media_type.to_string(),
        })
    }

    /// Parse image config JSON to extract entrypoint, cmd, env, etc.
    pub fn parse_config(data: &[u8]) -> Result<ImageConfig, OciError> {
        let root: RawImageRoot = serde_json::from_slice(data)
            .map_err(|e| OciError::ManifestParse(format!("invalid config JSON: {}", e)))?;

        let cfg = root.config.unwrap_or(RawContainerConfig {
            Entrypoint: None,
            Cmd: None,
            Env: None,
            WorkingDir: None,
            User: None,
            ExposedPorts: None,
        });

        let exposed_ports = if let Some(obj) = cfg.ExposedPorts {
            let mut ports = Vec::with_capacity(obj.len());
            for key in obj.keys() {
                let port = key
                    .split('/')
                    .next()
                    .ok_or_else(|| OciError::ManifestParse(format!("invalid exposed port '{}'", key)))?
                    .parse::<u16>()
                    .map_err(|e| OciError::ManifestParse(format!("invalid exposed port '{}': {}", key, e)))?;
                ports.push(port);
            }
            ports
        } else {
            Vec::new()
        };

        Ok(ImageConfig {
            entrypoint: cfg.Entrypoint.unwrap_or_default(),
            cmd: cfg.Cmd.unwrap_or_default(),
            env: cfg.Env.unwrap_or_default(),
            working_dir: cfg.WorkingDir.unwrap_or_default(),
            user: cfg.User.unwrap_or_default(),
            exposed_ports,
        })
    }

    /// Extract all layers of an image into a target directory (for rootfs preparation).
    pub fn extract_layers(
        &self,
        manifest: &ImageManifest,
        target: &Path,
    ) -> Result<(), OciError> {
        std::fs::create_dir_all(target)?;

        for (i, digest) in manifest.layer_digests.iter().enumerate() {
            let blob_path = self.blob_path(digest);
            if !blob_path.exists() {
                return Err(OciError::Blob(format!("missing layer blob: {}", digest)));
            }

            tracing::info!(
                "extracting layer {}/{}: {}",
                i + 1,
                manifest.layer_digests.len(),
                &digest[..19.min(digest.len())]
            );

            let file = std::fs::File::open(&blob_path)?;
            let reader: Box<dyn std::io::Read> = if is_gzip(&blob_path)? {
                Box::new(flate2::read::GzDecoder::new(file))
            } else {
                Box::new(file)
            };

            let mut archive = tar::Archive::new(reader);
            archive.set_preserve_permissions(true);
            archive.set_preserve_ownerships(false);
            archive.set_unpack_xattrs(false);
            archive.set_overwrite(true);

            for entry in archive
                .entries()
                .map_err(|e| OciError::Blob(format!("tar read error: {}", e)))?
            {
                let mut entry =
                    entry.map_err(|e| OciError::Blob(format!("tar entry error: {}", e)))?;
                let path = entry
                    .path()
                    .map_err(|e| OciError::Blob(format!("tar path error: {}", e)))?
                    .to_path_buf();
                let path_str = path.to_string_lossy();

                // Handle whiteout files (.wh.*)
                if let Some(filename) = path.file_name().and_then(|f| f.to_str()) {
                    if let Some(deleted_name) = filename.strip_prefix(".wh.") {
                        if deleted_name == ".wh..opq" {
                            if let Some(parent) = path.parent() {
                                let full_parent = target.join(parent);
                                if full_parent.exists() {
                                    let entries = std::fs::read_dir(&full_parent)
                                        .map_err(|e| OciError::Blob(format!("opaque whiteout read_dir failed for {}: {}", full_parent.display(), e)))?;
                                    for child in entries.flatten() {
                                        if let Err(e) = std::fs::remove_dir_all(child.path()) {
                                            tracing::warn!(path = %child.path().display(), "opaque whiteout cleanup failed: {}", e);
                                        }
                                    }
                                }
                            }
                        } else if let Some(parent) = path.parent() {
                            let deleted_path = target.join(parent).join(deleted_name);
                            // Try file first, then directory — whiteout target could be either
                            if std::fs::remove_file(&deleted_path).is_err() {
                                if let Err(e) = std::fs::remove_dir_all(&deleted_path) {
                                    tracing::debug!(path = %deleted_path.display(), "whiteout target not found (may not exist in lower layers): {}", e);
                                }
                            }
                        }
                        continue;
                    }
                }

                // Skip absolute paths and path traversal.
                // Check each component individually — "foo..bar" is safe, "../foo" is not.
                let has_traversal = path.components().any(|c| {
                    matches!(c, std::path::Component::ParentDir | std::path::Component::RootDir)
                });
                if has_traversal {
                    tracing::warn!(path = %path_str, "skipping tar entry with path traversal");
                    continue;
                }

                entry
                    .unpack_in(target)
                    .map_err(|e| OciError::Blob(format!("unpack error for {}: {}", path_str, e)))?;
            }
        }

        Ok(())
    }
}

fn is_gzip(path: &Path) -> Result<bool, OciError> {
    let mut f = std::fs::File::open(path)?;
    let mut magic = [0u8; 2];
    use std::io::Read;
    if f.read(&mut magic).map_err(OciError::Io)? == 2 {
        Ok(magic[0] == 0x1f && magic[1] == 0x8b)
    } else {
        Ok(false)
    }
}

fn sanitize_name(s: &str) -> String {
    s.replace('/', "_slash_").replace(':', "_colon_")
}

fn unsanitize_name(s: &str) -> String {
    let result = s.replace("_slash_", "/").replace("_colon_", ":");
    if result == s && s.contains('_') && !s.contains('/') {
        return s.replacen('_', "/", 1);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_docker_manifest() {
        let manifest_json = r#"{
            "schemaVersion": 2,
            "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
            "config": {
                "mediaType": "application/vnd.docker.container.image.v1+json",
                "size": 7023,
                "digest": "sha256:abc123"
            },
            "layers": [
                {
                    "mediaType": "application/vnd.docker.image.rootfs.diff.tar.gzip",
                    "size": 32654,
                    "digest": "sha256:layer1"
                },
                {
                    "mediaType": "application/vnd.docker.image.rootfs.diff.tar.gzip",
                    "size": 16724,
                    "digest": "sha256:layer2"
                }
            ]
        }"#;

        let manifest = ImageStore::parse_manifest(manifest_json.as_bytes())
            .expect("manifest should parse");
        assert_eq!(manifest.config_digest, "sha256:abc123");
        assert_eq!(manifest.layer_digests.len(), 2);
    }

    #[test]
    fn parse_image_config() {
        let config_json = r#"{
            "config": {
                "Env": ["PATH=/usr/local/sbin:/usr/local/bin", "NGINX_VERSION=1.25"],
                "Cmd": ["nginx", "-g", "daemon off;"],
                "WorkingDir": "/",
                "ExposedPorts": { "80/tcp": {} }
            }
        }"#;

        let config = ImageStore::parse_config(config_json.as_bytes())
            .expect("config should parse");
        assert_eq!(config.cmd, vec!["nginx", "-g", "daemon off;"]);
        assert_eq!(config.env.len(), 2);
        assert_eq!(config.exposed_ports, vec![80]);
    }

    #[test]
    fn blob_path_strips_prefix() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = ImageStore::new(tmp.path()).expect("store");
        let path = store.blob_path("sha256:abc123def456");
        assert!(path.to_string_lossy().ends_with("blobs/sha256/abc123def456"));
    }

    #[test]
    fn sanitize_roundtrip() {
        let name = "docker.io/library/nginx:latest";
        let sanitized = sanitize_name(name);
        assert!(!sanitized.contains('/'));
        assert!(!sanitized.contains(':'));
        let unsanitized = unsanitize_name(&sanitized);
        assert_eq!(unsanitized, name);
    }

    #[test]
    fn sanitize_simple_name() {
        let name = "alpine";
        let sanitized = sanitize_name(name);
        assert_eq!(sanitized, "alpine");
    }

    #[test]
    fn parse_manifest_missing_config() {
        let manifest_json = r#"{"schemaVersion": 2, "layers": []}"#;
        let result = ImageStore::parse_manifest(manifest_json.as_bytes());
        assert!(result.is_err());
    }

    #[test]
    fn parse_manifest_missing_layer_digest() {
        let manifest_json = r#"{
            "schemaVersion": 2,
            "config": { "digest": "sha256:cfg" },
            "layers": [{}, { "digest": "sha256:layer2" }]
        }"#;
        let err = ImageStore::parse_manifest(manifest_json.as_bytes()).expect_err("missing digest should fail");
        assert!(err.to_string().contains("missing layer digest"));
    }

    #[test]
    fn parse_config_minimal() {
        let config_json = r#"{"config": {}}"#;
        let config = ImageStore::parse_config(config_json.as_bytes())
            .expect("config should parse");
        assert!(config.cmd.is_empty());
        assert!(config.env.is_empty());
        assert!(config.exposed_ports.is_empty());
    }

    #[test]
    fn parse_config_with_entrypoint() {
        let config_json = r#"{
            "config": {
                "Entrypoint": ["/docker-entrypoint.sh"],
                "Cmd": ["nginx"]
            }
        }"#;
        let config = ImageStore::parse_config(config_json.as_bytes())
            .expect("config should parse");
        assert_eq!(config.entrypoint, vec!["/docker-entrypoint.sh"]);
        assert_eq!(config.cmd, vec!["nginx"]);
    }

    #[test]
    fn parse_config_exposed_ports_multiple() {
        let config_json = r#"{
            "config": {
                "ExposedPorts": { "80/tcp": {}, "443/tcp": {}, "8080/tcp": {} }
            }
        }"#;
        let config = ImageStore::parse_config(config_json.as_bytes())
            .expect("config should parse");
        let mut ports = config.exposed_ports.clone();
        ports.sort();
        assert_eq!(ports, vec![80, 443, 8080]);
    }
}
