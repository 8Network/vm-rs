//! Cloud-init seed ISO generation.
//!
//! Creates a seed ISO containing `meta-data`, `user-data`, and `network-config`
//! for cloud-init's NoCloud datasource. Each VM gets a unique seed ISO with its
//! SSH keys, network config, startup scripts, and health checks.

use std::path::Path;

use super::SetupError;

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
    /// Program to execute (path or name, no shell interpretation).
    pub command: String,
    /// Arguments to pass to the program (separate from command for safe quoting).
    pub args: Vec<String>,
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
    let tmp_dir = iso_path
        .parent()
        .ok_or_else(|| SetupError::Config("no parent directory for ISO path".into()))?
        .join(format!(".seed-{}", config.hostname));

    std::fs::create_dir_all(&tmp_dir).map_err(SetupError::Io)?;

    // Write meta-data
    let meta_data = format!(
        "instance-id: {hostname}\nlocal-hostname: {hostname}\n",
        hostname = config.hostname
    );
    std::fs::write(tmp_dir.join("meta-data"), &meta_data).map_err(SetupError::Io)?;

    // Write user-data
    let user_data = build_user_data(config);
    std::fs::write(tmp_dir.join("user-data"), &user_data).map_err(SetupError::Io)?;

    // Write network-config (v2)
    if !config.nics.is_empty() {
        let network_config = build_network_config(config);
        std::fs::write(tmp_dir.join("network-config"), &network_config).map_err(SetupError::Io)?;
    }

    // Create ISO
    create_iso_image(iso_path, &tmp_dir)?;

    // Clean up temp dir
    if let Err(e) = std::fs::remove_dir_all(&tmp_dir) {
        tracing::warn!(path = %tmp_dir.display(), "failed to clean up seed ISO temp dir: {}", e);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn build_user_data(config: &SeedConfig<'_>) -> String {
    let mut ud = String::from("#cloud-config\n");
    ud.push_str("ssh_authorized_keys:\n");
    ud.push_str(&format!("  - {}\n", config.ssh_pubkey));
    ud.push_str("disable_root: false\n");
    ud.push_str("runcmd:\n");

    // Mount VirtioFS volumes — use YAML array syntax so no shell quoting is needed
    // for most arguments, and explicitly quote paths/tags that may contain special chars.
    for vol in &config.volumes {
        let mount = shell_quote(&vol.mount_point);
        let tag = shell_quote(&vol.tag);
        ud.push_str(&format!("  - [\"mkdir\", \"-p\", {}]\n", mount));
        if vol.read_only {
            ud.push_str(&format!(
                "  - [\"mount\", \"-t\", \"virtiofs\", {tag}, {mount}, \"-o\", \"ro\"]\n",
                tag = tag,
                mount = mount,
            ));
        } else {
            ud.push_str(&format!(
                "  - [\"mount\", \"-t\", \"virtiofs\", {tag}, {mount}]\n",
                tag = tag,
                mount = mount,
            ));
        }
    }

    // Extra hosts — validate to prevent shell injection
    for (host, ip) in &config.extra_hosts {
        if !is_safe_hostname(host) || !is_safe_ip(ip) {
            tracing::error!(host = %host, ip = %ip, "rejecting unsafe /etc/hosts entry");
            continue;
        }
        ud.push_str(&format!("  - echo '{} {}' >> /etc/hosts\n", ip, host));
    }

    // Start process — use YAML array syntax with each argument as a separate quoted element.
    // env exports use `sh -c` with fully quoted assignment to avoid eval of values.
    if let Some(ref proc) = config.process {
        for (k, v) in &proc.env {
            if !is_safe_env_name(k) {
                tracing::error!(name = %k, "rejecting unsafe environment variable name");
                continue;
            }
            ud.push_str(&format!(
                "  - [\"sh\", \"-c\", \"export {}={}\"]\n",
                k,
                shell_quote(v)
            ));
        }
        // Build the exec array: program followed by each arg, all shell-quoted.
        let quoted_cmd = shell_quote(&proc.command);
        let quoted_args: Vec<String> = proc.args.iter().map(|a| shell_quote(a)).collect();
        if let Some(ref wd) = proc.workdir {
            // Change to workdir then exec — use sh -c so cd applies to exec.
            let mut exec_parts = vec![quoted_cmd.clone()];
            exec_parts.extend(quoted_args.clone());
            let exec_str = exec_parts.join(" ");
            ud.push_str(&format!(
                "  - [\"sh\", \"-c\", \"cd {} && exec {}\"]\n",
                shell_quote(wd),
                exec_str,
            ));
        } else {
            // No workdir: YAML list directly (cloud-init passes args without shell).
            let mut entries = vec![quoted_cmd];
            entries.extend(quoted_args);
            ud.push_str(&format!("  - [{}]\n", entries.join(", ")));
        }
    }

    // Health check — use sh -c with fully quoted command string.
    if let Some(ref hc) = config.healthcheck {
        let quoted_hc_cmd = shell_quote(&hc.command);
        ud.push_str(&format!(
            "  - [\"sh\", \"-c\", \"while true; do {} > /tmp/vmrs-health 2>&1 && echo healthy >> /tmp/vmrs-health || echo unhealthy >> /tmp/vmrs-health; sleep {}; done &\"]\n",
            quoted_hc_cmd, hc.interval_secs
        ));
    }

    // Readiness marker — VmManager watches the console log for this
    let ip_cmd = "hostname -I | awk '{print $1}'";
    ud.push_str(&format!(
        "  - echo \"{} $({})\"\n",
        crate::config::READY_MARKER,
        ip_cmd
    ));

    ud
}

fn build_network_config(config: &SeedConfig<'_>) -> String {
    let mut nc = String::from("version: 2\nethernets:\n");
    for nic in &config.nics {
        nc.push_str(&format!("  {}:\n", nic.name));
        nc.push_str(&format!("    addresses: [{}]\n", nic.ip));
        if let Some(ref gw) = nic.gateway {
            nc.push_str(&format!(
                "    routes:\n      - to: default\n        via: {}\n",
                gw
            ));
        }
    }
    nc
}

#[allow(unused_variables)] // used on macOS/Linux, not on unsupported platforms
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

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        return Err(SetupError::IsoCreation(format!(
            "seed ISO creation not yet supported on {}",
            std::env::consts::OS
        )));
    }

    #[allow(unreachable_code)]
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
        let ud = build_user_data(&config);
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
                args: vec![],
                workdir: Some("/opt/app".into()),
                env: vec![("PORT".into(), "8080".into())],
            }),
            volumes: vec![],
            healthcheck: None,
            extra_hosts: vec![],
        };
        let ud = build_user_data(&config);
        assert!(ud.contains("export PORT='8080'"));
        assert!(ud.contains("cd '/opt/app' && exec '/bin/app'"));
    }

    #[test]
    fn user_data_with_process_args() {
        let config = SeedConfig {
            hostname: "test-vm",
            ssh_pubkey: "ssh-ed25519 AAAA...",
            nics: vec![],
            process: Some(ProcessConfig {
                command: "/bin/app".into(),
                args: vec!["--config".into(), "/etc/app.conf".into()],
                workdir: None,
                env: vec![],
            }),
            volumes: vec![],
            healthcheck: None,
            extra_hosts: vec![],
        };
        let ud = build_user_data(&config);
        // Should produce YAML array list with program + args
        assert!(ud.contains("'/bin/app', '--config', '/etc/app.conf'"));
    }

    #[test]
    fn user_data_rejects_bad_env_name() {
        let config = SeedConfig {
            hostname: "test-vm",
            ssh_pubkey: "ssh-ed25519 AAAA...",
            nics: vec![],
            process: Some(ProcessConfig {
                command: "/bin/app".into(),
                args: vec![],
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
        let ud = build_user_data(&config);
        assert!(ud.contains("export GOOD="));
        assert!(!ud.contains("BAD;rm"));
    }

    #[test]
    fn user_data_shell_metacharacters_in_mount_and_command() {
        // Every dangerous value must appear only inside shell single-quotes so it cannot
        // be interpreted by a shell.  shell_quote wraps the string in '...', which means
        // the raw injection form (unquoted or preceded by a space) must NOT appear in the
        // output, while the properly-quoted form MUST appear.
        let config = SeedConfig {
            hostname: "test-vm",
            ssh_pubkey: "ssh-ed25519 AAAA...",
            nics: vec![],
            process: Some(ProcessConfig {
                command: "/bin/app; rm -rf /".into(),
                args: vec!["$(whoami)".into(), "arg with spaces".into()],
                workdir: Some("/work dir/path".into()),
                env: vec![("VAR".into(), "value$(id)".into())],
            }),
            volumes: vec![VolumeMountConfig {
                tag: "tag;evil".into(),
                mount_point: "/mnt/$(evil)".into(),
                read_only: false,
            }],
            healthcheck: Some(HealthCheckConfig {
                command: "curl http://localhost; rm -rf /".into(),
                interval_secs: 5,
                retries: 3,
            }),
            extra_hosts: vec![],
        };
        let ud = build_user_data(&config);

        // Mount tag: must appear shell-quoted (surrounded by single-quotes), never bare.
        assert!(
            ud.contains("'tag;evil'"),
            "mount tag must appear shell-quoted"
        );
        // The bare (unquoted) form with a leading space or comma would be injectable.
        assert!(
            !ud.contains(", tag;evil"),
            "mount tag must not appear as bare YAML element"
        );
        assert!(
            !ud.contains("\" tag;evil\""),
            "mount tag must not appear unquoted in string"
        );

        // Mount point: same rule.
        assert!(
            ud.contains("'/mnt/$(evil)'"),
            "mount point must appear shell-quoted"
        );
        assert!(
            !ud.contains(", /mnt/$(evil)"),
            "mount point must not appear as bare YAML element"
        );

        // Process command: the semicolon injection must be inside single-quotes.
        assert!(
            ud.contains("'/bin/app; rm -rf /'"),
            "command must appear shell-quoted"
        );

        // Args: $() injection must be inside single-quotes.
        assert!(ud.contains("'$(whoami)'"), "arg must appear shell-quoted");
        assert!(
            !ud.contains(", $(whoami)"),
            "arg must not appear as bare YAML element"
        );

        // Env value: $() injection must be inside single-quotes.
        assert!(
            ud.contains("'value$(id)'"),
            "env value must appear shell-quoted"
        );

        // Health check: the semicolon injection must be inside single-quotes.
        assert!(
            ud.contains("'curl http://localhost; rm -rf /'"),
            "healthcheck command must appear shell-quoted"
        );

        // Every runcmd line must start with "  - "
        let runcmd_lines: Vec<&str> = ud
            .lines()
            .skip_while(|l| *l != "runcmd:")
            .skip(1)
            .take_while(|l| l.starts_with("  - "))
            .collect();
        assert!(!runcmd_lines.is_empty(), "must have runcmd entries");
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
        let nc = build_network_config(&config);
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
        let nc = build_network_config(&config);
        assert!(nc.contains("eth0:"));
        assert!(!nc.contains("routes:"));
    }
}
