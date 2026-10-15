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

    pub fn validate(&self) -> Result<()> {
        if self.limits.agent_timeout_secs <= 0 {
            bail!("limits.agent_timeout_secs must be positive");
        }
        if self.limits.console_buffer_bytes == 0 {
            bail!("limits.console_buffer_bytes must be positive");
        }
        Ok(())
    }

    pub fn is_allowed(&self, vm: &str) -> bool {
        self.allowed_vms.iter().any(|a| a == "*" || a == vm)
    }
}
