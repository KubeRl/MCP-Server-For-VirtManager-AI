use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Contents of the TOML configuration file. Every field is optional; CLI flags
/// override what is read from here.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// libvirt connection URI.
    pub uri: String,
    /// Names of the VMs the AI may access. `"*"` allows every VM.
    /// An empty list means no VM is accessible.
    pub allowed_vms: Vec<String>,
    /// VMs the AI may start and stop. They must also be in `allowed_vms`;
    /// `"*"` means every allowed VM. Empty by default.
    pub power_vms: Vec<String>,
    /// Host directories whose files `copy_to_vm` may copy into a guest.
    /// Empty by default, which disables the tool.
    pub host_share_dirs: Vec<PathBuf>,
    /// Disables command execution, file writes, console input, keyboard input,
    /// screenshots and guest info queries.
    pub read_only: bool,
    pub limits: Limits,
    pub http: Http,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Default wall-clock limit for `exec` (overridable per call).
    pub exec_timeout_secs: u64,
    /// Max bytes returned per stream (stdout / stderr) by `exec` (overridable per call).
    pub max_output_bytes: usize,
    /// Max bytes returned by `read_file` (overridable per call).
    pub max_file_read_bytes: usize,
    /// Timeout for a single guest agent request.
    pub agent_timeout_secs: i32,
    /// Size of the per-VM serial console scrollback buffer.
    pub console_buffer_bytes: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Http {
    /// Address to listen on in HTTP mode.
    pub bind: String,
    /// Bearer token required in HTTP mode. Can also come from the
    /// `VIRTMANAGER_MCP_TOKEN` environment variable.
    pub token: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            uri: "qemu:///system".into(),
            allowed_vms: Vec::new(),
            power_vms: Vec::new(),
            host_share_dirs: Vec::new(),
            read_only: false,
            limits: Limits::default(),
            http: Http::default(),
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            exec_timeout_secs: 60,
            max_output_bytes: 1024 * 1024,
            max_file_read_bytes: 10 * 1024 * 1024,
            agent_timeout_secs: 10,
            console_buffer_bytes: 256 * 1024,
        }
    }
}

impl Default for Http {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8765".into(),
            token: None,
        }
    }
}

impl Config {
    pub fn default_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("virtmanager-mcp").join("config.toml"))
    }

    /// Loads `path`. A missing file is only an error when the path was given explicitly.
    pub fn load(path: &Path, explicit: bool) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("invalid config file {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => {
                tracing::warn!("config file {} not found, using defaults", path.display());
                Ok(Self::default())
            }
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    /// Checks limits and resolves `host_share_dirs` to canonical paths.
    pub fn validate(&mut self) -> Result<()> {
        if self.limits.agent_timeout_secs <= 0 {
            bail!("limits.agent_timeout_secs must be positive");
        }
        if self.limits.console_buffer_bytes == 0 {
            bail!("limits.console_buffer_bytes must be positive");
        }
        for dir in &mut self.host_share_dirs {
            let real = dir
                .canonicalize()
                .with_context(|| format!("host_share_dirs: cannot access {}", dir.display()))?;
            if !real.is_dir() {
                bail!("host_share_dirs: {} is not a directory", dir.display());
            }
            *dir = real;
        }
        Ok(())
    }

    pub fn is_allowed(&self, vm: &str) -> bool {
        self.allowed_vms.iter().any(|a| a == "*" || a == vm)
    }

    pub fn can_power(&self, vm: &str) -> bool {
        self.is_allowed(vm) && self.power_vms.iter().any(|a| a == "*" || a == vm)
    }

    /// Resolves `path` (symlinks and `..` included) and checks that it is a regular
    /// file inside one of `host_share_dirs`.
    pub fn share_file(&self, path: &str) -> Result<PathBuf> {
        let p = Path::new(path);
        if !p.is_absolute() {
            bail!("host_path must be absolute");
        }
        let real = p.canonicalize().with_context(|| format!("cannot access {path}"))?;
        if !self.host_share_dirs.iter().any(|d| real.starts_with(d)) {
            bail!("{path} is not inside host_share_dirs");
        }
        if !real.is_file() {
            bail!("{path} is not a regular file");
        }
        Ok(real)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_file_stays_inside_share_dirs() {
        let root = std::env::temp_dir().join(format!("virtmanager-mcp-test-{}", std::process::id()));
        let share = root.join("share");
        std::fs::create_dir_all(share.join("sub")).unwrap();
        std::fs::write(share.join("sub/ok.txt"), "x").unwrap();
        std::fs::write(root.join("secret.txt"), "x").unwrap();
        std::os::unix::fs::symlink(root.join("secret.txt"), share.join("link.txt")).unwrap();

        let mut config = Config { host_share_dirs: vec![share.clone()], ..Config::default() };
        config.validate().unwrap();
        let s = |p: &Path| p.to_str().unwrap().to_string();

        assert!(config.share_file(&s(&share.join("sub/ok.txt"))).is_ok());
        assert!(config.share_file(&s(&share.join("sub/../../secret.txt"))).is_err());
        assert!(config.share_file(&s(&share.join("link.txt"))).is_err());
        assert!(config.share_file(&s(&share.join("sub"))).is_err());
        assert!(config.share_file("sub/ok.txt").is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
