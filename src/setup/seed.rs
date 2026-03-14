//! Cloud-init seed ISO generation.
//!
//! Creates a seed ISO containing `meta-data`, `user-data`, and `network-config`
//! for cloud-init's NoCloud datasource. Each VM gets a unique seed ISO with its
//! SSH keys, network config, startup scripts, and health checks.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::SetupError;

static SEED_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Cloud-init seed configuration for a service VM.
#[derive(Debug)]
pub struct SeedConfig<'a> {
    /// VM hostname.
    pub hostname: &'a str,
    /// SSH public key for root access.
    pub ssh_pubkey: &'a str,
    /// Network interface configurations.
    pub nics: Vec<NicConfig>,
    /// Process to start after boot.
    pub process: Option<ProcessConfig>,
    /// Volume mounts (host dirs shared via VirtioFS).
    pub volumes: Vec<VolumeMountConfig>,
    /// Health check configuration.
    pub healthcheck: Option<HealthCheckConfig>,
    /// Additional /etc/hosts entries.
    pub extra_hosts: Vec<(String, String)>,
}

/// Network interface configuration for cloud-init.
#[derive(Debug, Clone)]
pub struct NicConfig {
    /// Interface name (eth0, eth1, ...).
    pub name: String,
    /// Static IP address with CIDR (e.g., "10.0.1.2/24").
    pub ip: String,
    /// Gateway IP (optional).
    pub gateway: Option<String>,
}

/// Process to start inside the VM after boot.
#[derive(Debug, Clone)]
pub struct ProcessConfig {
    /// Command to execute.
    pub command: String,
    /// Working directory.
    pub workdir: Option<String>,
    /// Environment variables.
    pub env: Vec<(String, String)>,
}

/// Volume mount configuration for cloud-init.
#[derive(Debug, Clone)]
pub struct VolumeMountConfig {
    /// VirtioFS tag (matches SharedDir.tag).
    pub tag: String,
    /// Mount point inside the VM.
    pub mount_point: String,
    /// Read-only mount.
    pub read_only: bool,
}

/// Health check configuration.
#[derive(Debug, Clone)]
pub struct HealthCheckConfig {
    /// Command to run for health check.
    pub command: String,
    /// Check interval in seconds.
    pub interval_secs: u32,
    /// Number of retries before marking unhealthy.
    pub retries: u32,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Create a cloud-init seed ISO for a service VM.
///
/// The ISO contains:
/// - `meta-data`: instance-id and hostname
/// - `user-data`: SSH keys, package config, startup scripts
/// - `network-config`: static IP assignment for each NIC
///
/// On macOS: uses `hdiutil makehybrid` to create the ISO.
/// On Linux: uses `genisoimage` or `mkisofs`.
pub fn create_seed_iso(iso_path: &Path, config: &SeedConfig<'_>) -> Result<(), SetupError> {
    if !is_safe_hostname(config.hostname) {
        return Err(SetupError::Config(format!(
            "invalid hostname '{}'",
            config.hostname
        )));
    }

    let parent = iso_path
        .parent()
        .ok_or_else(|| SetupError::Config("no parent directory for ISO path".into()))?;
    let counter = SEED_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let timestamp_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos();
    let tmp_dir = parent.join(format!(
        ".seed-{}-{}-{}-{}",
        config.hostname,
        std::process::id(),
        timestamp_nanos,
        counter
    ));

    std::fs::create_dir_all(&tmp_dir).map_err(SetupError::Io)?;

    let result = (|| {
        // Write meta-data
        let meta_data = format!(
            "instance-id: {hostname}\nlocal-hostname: {hostname}\n",
            hostname = config.hostname
        );
        std::fs::write(tmp_dir.join("meta-data"), &meta_data).map_err(SetupError::Io)?;

        // Write user-data
        let user_data = build_user_data(config)?;
        std::fs::write(tmp_dir.join("user-data"), &user_data).map_err(SetupError::Io)?;

        // Write network-config (v2)
        if !config.nics.is_empty() {
            let network_config = build_network_config(config)?;
            std::fs::write(tmp_dir.join("network-config"), &network_config)
                .map_err(SetupError::Io)?;
        }

        // Create ISO
        create_iso_image(iso_path, &tmp_dir)
    })();

    if let Err(e) = std::fs::remove_dir_all(&tmp_dir) {
        tracing::warn!(
            path = %tmp_dir.display(),
            "failed to clean up seed ISO temp dir: {}",
            e
        );
    }

    result
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn build_user_data(config: &SeedConfig<'_>) -> Result<String, SetupError> {
    validate_ssh_pubkey(config.ssh_pubkey)?;
    let mut ud = String::from("#cloud-config\n");
    ud.push_str("ssh_authorized_keys:\n");
    ud.push_str(&format!("  - {}\n", config.ssh_pubkey));
    ud.push_str("disable_root: false\n");
    ud.push_str("runcmd:\n");

    // Mount VirtioFS volumes
    for vol in &config.volumes {
        if !is_safe_mount_tag(&vol.tag) {
            return Err(SetupError::Config(format!(
                "invalid VirtioFS tag '{}'",
                vol.tag
            )));
        }
        if !is_safe_mount_point(&vol.mount_point) {
            return Err(SetupError::Config(format!(
                "invalid mount point '{}'",
                vol.mount_point
            )));
        }
        ud.push_str(&format!(
            "  - mkdir -p {mount} && mount -t virtiofs {tag} {mount}{ro}\n",
            mount = shell_quote(&vol.mount_point),
            tag = shell_quote(&vol.tag),
            ro = if vol.read_only { " -o ro" } else { "" },
        ));
    }

    // Extra hosts — validate to prevent shell injection
    for (host, ip) in &config.extra_hosts {
        if !is_safe_hostname(host) || !is_safe_ip(ip) {
            return Err(SetupError::Config(format!(
                "unsafe /etc/hosts entry: {} {}",
                ip, host
            )));
        }
        ud.push_str(&format!("  - echo '{} {}' >> /etc/hosts\n", ip, host));
    }

    // Start process
    if let Some(ref proc) = config.process {
        if !is_safe_shell_fragment(&proc.command) {
            return Err(SetupError::Config(
                "process command contains unsafe control characters".into(),
            ));
        }
        for (k, v) in &proc.env {
            if !is_safe_env_name(k) {
                return Err(SetupError::Config(format!(
                    "unsafe environment variable name '{}'",
                    k
                )));
            }
            ud.push_str(&format!("  - export {}={}\n", k, shell_quote(v)));
        }
        if let Some(ref wd) = proc.workdir {
            if !is_safe_mount_point(wd) {
                return Err(SetupError::Config(format!(
                    "invalid working directory '{}'",
                    wd
                )));
            }
            ud.push_str(&format!(
                "  - cd {} && sh -lc {}\n",
                shell_quote(wd),
                shell_quote(&proc.command)
            ));
        } else {
            ud.push_str(&format!("  - sh -lc {}\n", shell_quote(&proc.command)));
        }
    }

    // Health check
    if let Some(ref hc) = config.healthcheck {
        if !is_safe_shell_fragment(&hc.command) {
            return Err(SetupError::Config(
                "healthcheck command contains unsafe control characters".into(),
            ));
        }
        ud.push_str(&format!(
            "  - while true; do sh -lc {} > /tmp/vmrs-health 2>&1 && \
             echo 'healthy' >> /tmp/vmrs-health || \
             echo 'unhealthy' >> /tmp/vmrs-health; sleep {}; done &\n",
            shell_quote(&hc.command),
            hc.interval_secs
        ));
    }

    // Readiness marker — VmManager watches the console log for this
    let ip_cmd = "hostname -I | awk '{print $1}'";
    ud.push_str(&format!(
        "  - echo \"{} $({})\"\n",
        crate::config::READY_MARKER,
        ip_cmd
    ));

    Ok(ud)
}

fn build_network_config(config: &SeedConfig<'_>) -> Result<String, SetupError> {
    let mut nc = String::from("version: 2\nethernets:\n");
    for nic in &config.nics {
        if !is_safe_iface_name(&nic.name) {
            return Err(SetupError::Config(format!(
                "invalid interface name '{}'",
                nic.name
            )));
        }
        if !is_safe_cidr(&nic.ip) {
            return Err(SetupError::Config(format!(
                "invalid interface address '{}'",
                nic.ip
            )));
        }
        nc.push_str(&format!("  {}:\n", nic.name));
        nc.push_str(&format!("    addresses: [{}]\n", nic.ip));
        if let Some(ref gw) = nic.gateway {
            if !is_safe_ip(gw) {
                return Err(SetupError::Config(format!("invalid gateway '{}'", gw)));
            }
            nc.push_str(&format!(
                "    routes:\n      - to: default\n        via: {}\n",
                gw
            ));
        }
    }
    Ok(nc)
}

fn create_iso_image(iso_path: &Path, source_dir: &Path) -> Result<(), SetupError> {
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("hdiutil")
            .args(["makehybrid", "-o"])
            .arg(iso_path)
            .arg(source_dir)
            .args(["-joliet", "-iso", "-default-volume-name", "cidata"])
            .output()
            .map_err(SetupError::Io)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SetupError::IsoCreation(format!(
                "hdiutil makehybrid failed (exit {}): {}",
                output.status,
                stderr.trim()
            )));
        }
    }

    #[cfg(target_os = "linux")]
    {
        // Try genisoimage first, fall back to mkisofs
        let result = std::process::Command::new("genisoimage")
            .args(["-output"])
            .arg(iso_path)
            .args(["-volid", "cidata", "-joliet", "-rock"])
            .arg(source_dir)
            .output();

        match result {
            Ok(ref out) if out.status.success() => {}
            _ => {
                let output = std::process::Command::new("mkisofs")
                    .args(["-output"])
                    .arg(iso_path)
                    .args(["-volid", "cidata", "-joliet", "-rock"])
                    .arg(source_dir)
                    .output()
                    .map_err(SetupError::Io)?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(SetupError::IsoCreation(format!(
                        "neither genisoimage nor mkisofs succeeded. \
                         Install: apt install genisoimage. Last error: {}",
                        stderr.trim()
                    )));
                }
            }
        }
    }

    Ok(())
}

/// Quote a string for safe embedding in a shell command.
///
/// Uses single quotes, which prevent ALL shell interpretation. The only
/// character that needs escaping inside single quotes is the single quote
/// itself, handled via the standard `'\''` idiom (end quote, escaped quote,
/// restart quote).
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Validate an environment variable name (POSIX: uppercase letters, digits, underscore; must not start with digit).
fn is_safe_env_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !s.starts_with(|c: char| c.is_ascii_digit())
}

/// Validate a hostname contains only safe characters (alphanumeric, hyphens, dots).
fn is_safe_hostname(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
}

/// Validate an IP address contains only safe characters (digits, dots, colons for IPv6).
fn is_safe_ip(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 45
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() || c == '.' || c == ':')
}

fn is_safe_cidr(s: &str) -> bool {
    let Some((ip, prefix)) = s.split_once('/') else {
        return false;
    };
    if !is_safe_ip(ip) {
        return false;
    }
    matches!(prefix.parse::<u8>(), Ok(bits) if bits <= 128)
}

fn is_safe_mount_tag(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

fn is_safe_mount_point(s: &str) -> bool {
    s.starts_with('/')
        && !s.contains('\0')
        && !s.contains('\n')
        && !s.contains('\r')
        && s.len() <= 1024
}

fn is_safe_iface_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 15
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn is_safe_shell_fragment(s: &str) -> bool {
    !s.contains('\0') && !s.contains('\n') && !s.contains('\r')
}

fn validate_ssh_pubkey(s: &str) -> Result<(), SetupError> {
    if s.contains('\0') || s.contains('\n') || s.contains('\r') {
        return Err(SetupError::Config(
            "SSH public key must be a single line without NUL bytes".into(),
        ));
    }

    let mut parts = s.split_whitespace();
    let Some(key_type) = parts.next() else {
        return Err(SetupError::Config("SSH public key is empty".into()));
    };
    let Some(key_material) = parts.next() else {
        return Err(SetupError::Config(
            "SSH public key is missing key material".into(),
        ));
    };

    if !key_type.starts_with("ssh-") && !key_type.starts_with("ecdsa-") {
        return Err(SetupError::Config(format!(
            "unsupported SSH public key type '{}'",
            key_type
        )));
    }
    if key_material.is_empty() {
        return Err(SetupError::Config(
            "SSH public key is missing key material".into(),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── shell_quote ──────────────────────────────────────────────────────

    #[test]
    fn shell_quote_simple() {
        assert_eq!(shell_quote("hello"), "'hello'");
    }

    #[test]
    fn shell_quote_with_spaces() {
        assert_eq!(shell_quote("hello world"), "'hello world'");
    }

    #[test]
    fn shell_quote_with_single_quote() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn shell_quote_with_dollar() {
        // Single quotes prevent $() expansion
        assert_eq!(shell_quote("$(rm -rf /)"), "'$(rm -rf /)'");
    }

    #[test]
    fn shell_quote_with_backticks() {
        assert_eq!(shell_quote("`whoami`"), "'`whoami`'");
    }

    #[test]
    fn shell_quote_empty() {
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn shell_quote_with_newline() {
        assert_eq!(shell_quote("a\nb"), "'a\nb'");
    }

    #[test]
    fn shell_quote_with_semicolon() {
        assert_eq!(shell_quote("a; rm -rf /"), "'a; rm -rf /'");
    }

    // ── is_safe_hostname ─────────────────────────────────────────────────

    #[test]
    fn hostname_valid() {
        assert!(is_safe_hostname("my-host.local"));
        assert!(is_safe_hostname("a"));
        assert!(is_safe_hostname("web-01"));
    }

    #[test]
    fn hostname_empty() {
        assert!(!is_safe_hostname(""));
    }

    #[test]
    fn hostname_rejects_spaces() {
        assert!(!is_safe_hostname("my host"));
    }

    #[test]
    fn hostname_rejects_shell_chars() {
        assert!(!is_safe_hostname("host;rm -rf /"));
        assert!(!is_safe_hostname("host$(whoami)"));
        assert!(!is_safe_hostname("host'"));
    }

    #[test]
    fn hostname_rejects_too_long() {
        let long = "a".repeat(254);
        assert!(!is_safe_hostname(&long));
    }

    #[test]
    fn create_seed_iso_rejects_invalid_hostname_before_running_tools() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let iso_path = tmp.path().join("seed.iso");
        let config = SeedConfig {
            hostname: "../bad-host",
            ssh_pubkey: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAItest test@test",
            nics: vec![],
            process: None,
            volumes: vec![],
            healthcheck: None,
            extra_hosts: vec![],
        };

        let err = create_seed_iso(&iso_path, &config)
            .expect_err("invalid hostname should fail before ISO tool invocation");
        assert!(
            err.to_string().contains("invalid hostname"),
            "expected invalid hostname error, got: {}",
            err
        );
    }

    // ── is_safe_ip ───────────────────────────────────────────────────────

    #[test]
    fn ip_valid_v4() {
        assert!(is_safe_ip("192.168.1.1"));
        assert!(is_safe_ip("10.0.0.1"));
    }

    #[test]
    fn ip_valid_v6() {
        assert!(is_safe_ip("::1"));
        assert!(is_safe_ip("fe80::1"));
        assert!(is_safe_ip("2001:db8::1"));
    }

    #[test]
    fn ip_empty() {
        assert!(!is_safe_ip(""));
    }

    #[test]
    fn ip_rejects_shell_chars() {
        assert!(!is_safe_ip("1.1.1.1; rm -rf /"));
        assert!(!is_safe_ip("$(whoami)"));
    }

    // ── is_safe_env_name ─────────────────────────────────────────────────

    #[test]
    fn env_name_valid() {
        assert!(is_safe_env_name("PATH"));
        assert!(is_safe_env_name("MY_VAR_123"));
        assert!(is_safe_env_name("_PRIVATE"));
    }

    #[test]
    fn env_name_rejects_empty() {
        assert!(!is_safe_env_name(""));
    }

    #[test]
    fn env_name_rejects_leading_digit() {
        assert!(!is_safe_env_name("1BAD"));
    }

    #[test]
    fn env_name_rejects_shell_injection() {
        assert!(!is_safe_env_name("FOO;rm -rf /"));
        assert!(!is_safe_env_name("FOO=$(whoami)"));
        assert!(!is_safe_env_name("FOO BAR"));
    }

    // ── build_user_data ──────────────────────────────────────────────────

    #[test]
    fn user_data_basic_structure() {
        let config = SeedConfig {
            hostname: "test-vm",
            ssh_pubkey: "ssh-ed25519 AAAA...",
            nics: vec![],
            process: None,
            volumes: vec![],
            healthcheck: None,
            extra_hosts: vec![],
        };
        let ud = build_user_data(&config).expect("user-data");
        assert!(ud.starts_with("#cloud-config\n"));
        assert!(ud.contains("ssh-ed25519 AAAA..."));
        assert!(ud.contains("VMRS_READY"));
    }

    #[test]
    fn user_data_with_process_env() {
        let config = SeedConfig {
            hostname: "test-vm",
            ssh_pubkey: "ssh-ed25519 AAAA...",
            nics: vec![],
            process: Some(ProcessConfig {
                command: "/bin/app".into(),
                workdir: Some("/opt/app".into()),
                env: vec![("PORT".into(), "8080".into())],
            }),
            volumes: vec![],
            healthcheck: None,
            extra_hosts: vec![],
        };
        let ud = build_user_data(&config).expect("user-data");
        assert!(ud.contains("export PORT='8080'"));
        assert!(ud.contains("cd '/opt/app' && sh -lc '/bin/app'"));
    }

    #[test]
    fn user_data_rejects_bad_env_name() {
        let config = SeedConfig {
            hostname: "test-vm",
            ssh_pubkey: "ssh-ed25519 AAAA...",
            nics: vec![],
            process: Some(ProcessConfig {
                command: "/bin/app".into(),
                workdir: None,
                env: vec![
                    ("GOOD".into(), "ok".into()),
                    ("BAD;rm".into(), "evil".into()),
                ],
            }),
            volumes: vec![],
            healthcheck: None,
            extra_hosts: vec![],
        };
        let err = build_user_data(&config).expect_err("invalid env name should fail");
        assert!(err.to_string().contains("unsafe environment variable name"));
    }

    // ── build_network_config ─────────────────────────────────────────────

    #[test]
    fn network_config_static_ip() {
        let config = SeedConfig {
            hostname: "test-vm",
            ssh_pubkey: "ssh-ed25519 AAAA...",
            nics: vec![NicConfig {
                name: "eth0".into(),
                ip: "10.0.1.2/24".into(),
                gateway: Some("10.0.1.1".into()),
            }],
            process: None,
            volumes: vec![],
            healthcheck: None,
            extra_hosts: vec![],
        };
        let nc = build_network_config(&config).expect("network-config");
        assert!(nc.contains("version: 2"));
        assert!(nc.contains("eth0:"));
        assert!(nc.contains("addresses: [10.0.1.2/24]"));
        assert!(nc.contains("via: 10.0.1.1"));
    }

    #[test]
    fn network_config_no_gateway() {
        let config = SeedConfig {
            hostname: "test-vm",
            ssh_pubkey: "ssh-ed25519 AAAA...",
            nics: vec![NicConfig {
                name: "eth0".into(),
                ip: "10.0.1.2/24".into(),
                gateway: None,
            }],
            process: None,
            volumes: vec![],
            healthcheck: None,
            extra_hosts: vec![],
        };
        let nc = build_network_config(&config).expect("network-config");
        assert!(nc.contains("eth0:"));
        assert!(!nc.contains("routes:"));
    }
}
