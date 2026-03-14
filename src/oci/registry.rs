//! OCI Distribution Spec registry client -- pull images via HTTP.
//!
//! Supports Docker Hub and any OCI-compliant registry.
//! No Docker daemon needed. Just HTTP requests.
//!
//! Flow:
//! 1. Resolve image reference -> (registry, repository, tag)
//! 2. Authenticate (token-based, Docker Hub uses auth.docker.io)
//! 3. GET manifest (may be manifest list -> resolve to platform)
//! 4. GET config blob
//! 5. GET layer blobs (parallel, skip if already cached)

use serde::Deserialize;

use super::store::{ImageManifest, ImageStore};

#[derive(Debug, Clone)]
enum RegistryAuth {
    Anonymous,
    Bearer(String),
    Basic(String),
}

/// Parsed image reference: registry/repo:tag
#[derive(Debug, Clone)]
pub struct ImageRef {
    pub registry: String,
    pub repository: String,
    pub tag: String,
}

// ---------------------------------------------------------------------------
// Typed JSON response structs (replacing serde_json::Value)
// ---------------------------------------------------------------------------

/// Docker Hub / OCI token response.
#[derive(Deserialize)]
struct TokenResponse {
    token: Option<String>,
    access_token: Option<String>,
}

impl TokenResponse {
    fn into_token(self) -> Option<String> {
        self.token.or(self.access_token)
    }
}

/// A single entry in a manifest list / OCI index.
#[derive(Deserialize)]
struct ManifestListEntry {
    digest: String,
    platform: Option<ManifestPlatform>,
}

/// Platform descriptor in a manifest list entry.
#[derive(Deserialize)]
struct ManifestPlatform {
    os: Option<String>,
    architecture: Option<String>,
}

/// Top-level manifest list / OCI index.
#[derive(Deserialize)]
struct ManifestList {
    manifests: Vec<ManifestListEntry>,
}

/// Docker config.json `auths` section.
#[derive(Deserialize)]
struct DockerConfig {
    auths: Option<std::collections::HashMap<String, DockerAuthEntry>>,
}

/// Single entry in Docker config.json `auths`.
#[derive(Deserialize)]
struct DockerAuthEntry {
    auth: Option<String>,
}

/// Pull an image from a registry into the local store.
pub async fn pull(image: &str, store: &ImageStore) -> Result<ImageManifest, OciError> {
    let image_ref = parse_image_ref(image);
    let client = reqwest::Client::builder()
        .user_agent("vm-rs/0.1")
        .build()
        .map_err(|e| OciError::Http(format!("failed to create HTTP client: {}", e)))?;

    tracing::info!("pulling {} from {}", image, image_ref.registry);

    // 1. Get auth token
    let auth = authenticate(&client, &image_ref).await?;

    // 2. Fetch manifest
    let manifest_bytes = fetch_manifest(&client, &image_ref, &auth).await?;

    // 3. Parse manifest (handle manifest list if needed)
    let manifest = match ImageStore::parse_manifest(&manifest_bytes) {
        Ok(m) => {
            store.put_manifest(&image_ref.repository, &image_ref.tag, &manifest_bytes)?;
            m
        }
        Err(OciError::ManifestList) => {
            // Fat manifest -- resolve to our platform
            let platform_digest = resolve_platform_manifest(&manifest_bytes)?;
            let platform_ref = ImageRef {
                tag: platform_digest.clone(),
                ..image_ref.clone()
            };
            let platform_bytes = fetch_manifest(&client, &platform_ref, &auth).await?;
            store.put_manifest(&image_ref.repository, &image_ref.tag, &platform_bytes)?;
            ImageStore::parse_manifest(&platform_bytes)?
        }
        Err(e) => return Err(e),
    };

    // 4. Fetch config blob
    if !store.has_blob(&manifest.config_digest) {
        tracing::debug!("pulling config {}", &manifest.config_digest[..19.min(manifest.config_digest.len())]);
        let config_data = fetch_blob(&client, &image_ref, &manifest.config_digest, &auth).await?;
        store.put_blob(&manifest.config_digest, &config_data)?;
    }

    // 5. Fetch layer blobs (skip cached)
    let total = manifest.layer_digests.len();
    for (i, digest) in manifest.layer_digests.iter().enumerate() {
        if store.has_blob(digest) {
            tracing::debug!("layer {}/{}: cached", i + 1, total);
            continue;
        }

        tracing::info!("pulling layer {}/{}: {}..{}", i + 1, total, &digest[..19.min(digest.len())], &digest[digest.len().saturating_sub(4)..]);
        let data = fetch_blob(&client, &image_ref, digest, &auth).await?;
        store.put_blob(digest, &data)?;
    }

    tracing::info!("pull complete: {} layers", total);
    Ok(manifest)
}

/// Parse "nginx:latest" or "docker.io/library/nginx:1.25" into components.
pub fn parse_image_ref(image: &str) -> ImageRef {
    // Handle digest references: image@sha256:... → tag = sha256:...
    let (image_no_digest, digest) = if let Some(at) = image.find('@') {
        (&image[..at], Some(&image[at + 1..]))
    } else {
        (image, None)
    };

    let first_part = image_no_digest.split('/').next().unwrap_or(image_no_digest);
    // A registry is present if the first path segment contains a dot OR a colon (port)
    let has_registry = image_no_digest.contains('/')
        && (first_part.contains('.') || first_part.contains(':'));

    let (registry, rest) = if has_registry {
        let slash = image_no_digest.find('/').expect("checked above");
        (&image_no_digest[..slash], &image_no_digest[slash + 1..])
    } else {
        ("registry-1.docker.io", image_no_digest)
    };

    // Split repo:tag — but only if there's no digest (digest already extracted above)
    let (repo, tag) = if let Some(d) = digest {
        (rest, d)
    } else if let Some((r, t)) = rest.rsplit_once(':') {
        (r, t)
    } else {
        (rest, "latest")
    };

    // Docker Hub official images need "library/" prefix
    let repo = if registry == "registry-1.docker.io" && !repo.contains('/') {
        format!("library/{}", repo)
    } else {
        repo.to_string()
    };

    ImageRef {
        registry: registry.to_string(),
        repository: repo,
        tag: tag.to_string(),
    }
}

async fn authenticate(
    client: &reqwest::Client,
    image_ref: &ImageRef,
) -> Result<RegistryAuth, OciError> {
    if image_ref.registry == "registry-1.docker.io" {
        let url = format!(
            "https://auth.docker.io/token?service=registry.docker.io&scope=repository:{}:pull",
            image_ref.repository
        );
        let resp = client.get(&url).send().await
            .map_err(|e| OciError::Http(format!("auth request failed: {}", e)))?
            .error_for_status()
            .map_err(|e| OciError::Auth(format!("auth failed: {}", e)))?;
        let token_resp: TokenResponse = resp.json().await
            .map_err(|e| OciError::Auth(format!("invalid auth response: {}", e)))?;
        let token = token_resp.into_token()
            .ok_or_else(|| OciError::Auth("no token in auth response".into()))?;
        tracing::debug!(registry = %image_ref.registry, repository = %image_ref.repository, "using bearer token auth");
        Ok(RegistryAuth::Bearer(token))
    } else {
        // Check ~/.docker/config.json for stored credentials
        if let Some(basic_auth) = read_docker_config_auth(&image_ref.registry) {
            tracing::debug!(registry = %image_ref.registry, "found Docker config credentials");
            let url = format!("https://{}/v2/", image_ref.registry);
            let resp = client.get(&url).send().await
                .map_err(|e| OciError::Http(format!("registry probe failed: {}", e)))?;
            if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
                if let Some(header_value) = resp.headers().get("www-authenticate") {
                    match header_value.to_str() {
                        Ok(challenge) => {
                            if let Some(token_url) =
                                parse_www_authenticate(challenge, &image_ref.repository)
                            {
                                let resp = client
                                    .get(&token_url)
                                    .header("Authorization", format!("Basic {}", basic_auth))
                                    .send()
                                    .await
                                    .map_err(|e| OciError::Auth(format!("token exchange failed: {}", e)))?
                                    .error_for_status()
                                    .map_err(|e| OciError::Auth(format!("token exchange failed: {}", e)))?;
                                let token_resp: TokenResponse = resp.json().await
                                    .map_err(|e| OciError::Auth(format!("invalid token response: {}", e)))?;
                                if let Some(token) = token_resp.into_token() {
                                    tracing::debug!(registry = %image_ref.registry, "registry exchanged Docker config credentials for bearer token");
                                    return Ok(RegistryAuth::Bearer(token));
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(registry = %image_ref.registry, error = %e, "registry returned a non-UTF8 WWW-Authenticate header");
                        }
                    }
                }
            }
            tracing::info!(registry = %image_ref.registry, "registry did not offer bearer token exchange; falling back to basic auth");
            return Ok(RegistryAuth::Basic(basic_auth));
        }
        tracing::info!(registry = %image_ref.registry, "no registry credentials found; attempting anonymous pull");
        Ok(RegistryAuth::Anonymous)
    }
}

async fn fetch_manifest(
    client: &reqwest::Client,
    image_ref: &ImageRef,
    auth: &RegistryAuth,
) -> Result<Vec<u8>, OciError> {
    let url = format!(
        "https://{}/v2/{}/manifests/{}",
        image_ref.registry, image_ref.repository, image_ref.tag
    );

    let mut req = client.get(&url).header(
        "Accept",
        "application/vnd.docker.distribution.manifest.v2+json, \
         application/vnd.oci.image.manifest.v1+json, \
         application/vnd.docker.distribution.manifest.list.v2+json, \
         application/vnd.oci.image.index.v1+json",
    );

    if let Some(header) = format_auth_header(auth) {
        req = req.header("Authorization", header);
    }

    let resp = req.send().await
        .map_err(|e| OciError::Http(format!("manifest fetch failed: {}", e)))?
        .error_for_status()
        .map_err(|e| OciError::Http(format!("manifest fetch failed: {}", e)))?;
    Ok(resp.bytes().await
        .map_err(|e| OciError::Http(format!("failed to read manifest: {}", e)))?.to_vec())
}

async fn fetch_blob(
    client: &reqwest::Client,
    image_ref: &ImageRef,
    digest: &str,
    auth: &RegistryAuth,
) -> Result<Vec<u8>, OciError> {
    let url = format!(
        "https://{}/v2/{}/blobs/{}",
        image_ref.registry, image_ref.repository, digest
    );

    let mut req = client.get(&url);
    if let Some(header) = format_auth_header(auth) {
        req = req.header("Authorization", header);
    }

    let resp = req.send().await
        .map_err(|e| OciError::Http(format!("blob fetch failed: {}", e)))?
        .error_for_status()
        .map_err(|e| OciError::Http(format!("blob fetch failed: {}", e)))?;
    Ok(resp.bytes().await
        .map_err(|e| OciError::Http(format!("failed to read blob: {}", e)))?.to_vec())
}

fn resolve_platform_manifest(manifest_list_bytes: &[u8]) -> Result<String, OciError> {
    let list: ManifestList = serde_json::from_slice(manifest_list_bytes)
        .map_err(|e| OciError::ManifestParse(format!("invalid manifest list JSON: {}", e)))?;

    let target_arch = if cfg!(target_arch = "aarch64") { "arm64" } else { "amd64" };

    // Exact match: linux + target architecture
    for entry in &list.manifests {
        if let Some(ref platform) = entry.platform {
            let os = platform.os.as_deref().unwrap_or("");
            let arch = platform.architecture.as_deref().unwrap_or("");
            if os == "linux" && arch == target_arch {
                return Ok(entry.digest.clone());
            }
        }
    }

    Err(OciError::ManifestParse(format!(
        "no linux/{} manifest found in manifest list",
        target_arch
    )))
}

fn format_auth_header(auth: &RegistryAuth) -> Option<String> {
    match auth {
        RegistryAuth::Anonymous => None,
        RegistryAuth::Bearer(token) => Some(format!("Bearer {}", token)),
        RegistryAuth::Basic(token) => Some(format!("Basic {}", token)),
    }
}

fn read_docker_config_auth(registry: &str) -> Option<String> {
    let home = match std::env::var("HOME") {
        Ok(home) => home,
        Err(e) => {
            tracing::debug!(error = %e, "HOME is not set; skipping Docker config credential lookup");
            return None;
        }
    };
    let config_path = std::path::Path::new(&home).join(".docker/config.json");
    let content = match std::fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(path = %config_path.display(), "failed to read Docker config: {}", e);
            return None;
        }
    };
    let config: DockerConfig = match serde_json::from_str(&content) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(path = %config_path.display(), "failed to parse Docker config: {}", e);
            return None;
        }
    };
    let auths = config.auths?;

    let auth_entry = auths.get(registry)
        .or_else(|| auths.get(&format!("https://{}", registry)))
        .or_else(|| auths.get(&format!("https://{}/v2/", registry)))
        .or_else(|| auths.get(&format!("https://{}/v1/", registry)));

    let auth_str = auth_entry?.auth.as_deref()?;
    if auth_str.is_empty() { return None; }
    Some(auth_str.to_string())
}

fn parse_www_authenticate(header: &str, repository: &str) -> Option<String> {
    let header = header.strip_prefix("Bearer ")?;
    let mut realm = None;
    let mut service = None;

    for part in header.split(',') {
        let part = part.trim();
        if let Some(val) = part.strip_prefix("realm=") {
            realm = Some(val.trim_matches('"').to_string());
        } else if let Some(val) = part.strip_prefix("service=") {
            service = Some(val.trim_matches('"').to_string());
        }
    }

    let realm = realm?;
    let mut url = format!("{}?scope=repository:{}:pull", realm, repository);
    if let Some(svc) = service {
        url = format!("{}&service={}", url, svc);
    }
    Some(url)
}

/// OCI registry/store errors.
#[derive(Debug, thiserror::Error)]
pub enum OciError {
    #[error("HTTP error: {0}")]
    Http(String),

    #[error("authentication failed: {0}")]
    Auth(String),

    #[error("manifest parse error: {0}")]
    ManifestParse(String),

    #[error("manifest list requires platform resolution")]
    ManifestList,

    #[error("blob error: {0}")]
    Blob(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_image() {
        let r = parse_image_ref("nginx:latest");
        assert_eq!(r.registry, "registry-1.docker.io");
        assert_eq!(r.repository, "library/nginx");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn parse_image_no_tag() {
        let r = parse_image_ref("redis");
        assert_eq!(r.repository, "library/redis");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn parse_user_image() {
        let r = parse_image_ref("myuser/myapp:v1.2");
        assert_eq!(r.registry, "registry-1.docker.io");
        assert_eq!(r.repository, "myuser/myapp");
        assert_eq!(r.tag, "v1.2");
    }

    #[test]
    fn parse_custom_registry() {
        let r = parse_image_ref("ghcr.io/owner/repo:sha-abc");
        assert_eq!(r.registry, "ghcr.io");
        assert_eq!(r.repository, "owner/repo");
        assert_eq!(r.tag, "sha-abc");
    }

    #[test]
    fn parse_digest_reference() {
        let r = parse_image_ref("nginx@sha256:abc123");
        assert_eq!(r.repository, "library/nginx");
        // digest is in the tag field
        assert!(r.tag.contains("sha256"));
    }

    #[test]
    fn parse_registry_with_port() {
        let r = parse_image_ref("localhost:5000/myapp:latest");
        assert_eq!(r.registry, "localhost:5000");
        assert_eq!(r.repository, "myapp");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn parse_deeply_nested_repo() {
        let r = parse_image_ref("ghcr.io/org/team/app:v1");
        assert_eq!(r.registry, "ghcr.io");
        assert_eq!(r.repository, "org/team/app");
        assert_eq!(r.tag, "v1");
    }
}
