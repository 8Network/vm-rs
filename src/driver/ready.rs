use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Incremental readiness-marker scanner for serial logs.
#[derive(Debug, Default)]
pub(crate) struct ReadyMarkerCache {
    ready_ip: Option<String>,
    scan_offset: u64,
    tail: String,
}

impl ReadyMarkerCache {
    #[cfg(test)]
    pub(crate) fn ready_ip(&self) -> Option<&str> {
        self.ready_ip.as_deref()
    }

    pub(crate) fn scan(&mut self, log_path: &Path) -> Option<String> {
        if let Some(ip) = &self.ready_ip {
            return Some(ip.clone());
        }

        let mut file = match File::open(log_path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                tracing::warn!(
                    path = %log_path.display(),
                    "failed to open serial log while checking readiness: {}",
                    e
                );
                return None;
            }
        };

        let file_len = match file.metadata() {
            Ok(meta) => meta.len(),
            Err(e) => {
                tracing::warn!(
                    path = %log_path.display(),
                    "failed to stat serial log while checking readiness: {}",
                    e
                );
                return None;
            }
        };
        if file_len < self.scan_offset {
            self.scan_offset = 0;
            self.tail.clear();
        }

        if let Err(e) = file.seek(SeekFrom::Start(self.scan_offset)) {
            tracing::warn!(
                path = %log_path.display(),
                offset = self.scan_offset,
                "failed to seek serial log while checking readiness: {}",
                e
            );
            return None;
        }

        let mut buf = Vec::new();
        if let Err(e) = file.read_to_end(&mut buf) {
            tracing::warn!(
                path = %log_path.display(),
                "failed to read serial log while checking readiness: {}",
                e
            );
            return None;
        }
        self.scan_offset = file_len;

        if buf.is_empty() && self.tail.is_empty() {
            return None;
        }

        let chunk = String::from_utf8_lossy(&buf);
        let mut combined = String::with_capacity(self.tail.len() + chunk.len());
        combined.push_str(&self.tail);
        combined.push_str(&chunk);

        if let Some(ip) = parse_ready_marker(&combined) {
            self.ready_ip = Some(ip.clone());
            self.tail.clear();
            return Some(ip);
        }

        self.tail = trailing_overlap(&combined);
        None
    }
}

pub(crate) fn check_ready_marker(log_path: &Path) -> Option<String> {
    let content = match std::fs::read_to_string(log_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(path = %log_path.display(), "failed to read serial log: {}", e);
            return None;
        }
    };
    parse_ready_marker(&content)
}

fn parse_ready_marker(content: &str) -> Option<String> {
    let pos = content.find(crate::config::READY_MARKER)?;
    let after = &content[pos + crate::config::READY_MARKER.len()..];
    let ip = after.split_whitespace().next()?.trim().to_string();
    if ip.is_empty() {
        None
    } else {
        Some(ip)
    }
}

fn trailing_overlap(content: &str) -> String {
    let overlap = crate::config::READY_MARKER.len() + 64;
    let mut chars = content.chars().rev().take(overlap).collect::<Vec<_>>();
    chars.reverse();
    chars.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ready_marker_extracts_ip() {
        assert_eq!(
            parse_ready_marker("noise\nVMRS_READY 10.0.0.2\n"),
            Some("10.0.0.2".into())
        );
    }

    #[test]
    fn cache_detects_split_marker_across_reads() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("serial.log");
        std::fs::write(&path, "vmrs VMRS_RE").expect("write partial marker");

        let mut cache = ReadyMarkerCache::default();
        assert_eq!(cache.scan(&path), None);

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open for append");
        use std::io::Write;
        writeln!(file, "ADY 10.0.0.2").expect("append marker remainder");

        assert_eq!(cache.scan(&path), Some("10.0.0.2".into()));
        assert_eq!(cache.ready_ip(), Some("10.0.0.2"));
    }
}
