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
    /// Also lets `copy_to_vm` read from the directory the server was started in.
    /// Claude Code starts stdio servers in the session's project directory.
    pub share_project_dir: bool,
    /// Disables command execution, file writes, console input, keyboard input,
    /// screenshots and guest info queries.
    pub read_only: bool,
    pub limits: Limits,
    pub http: Http,
    pub manage: Manage,
}

/// VM management (create, delete, reconfigure, snapshots). Everything is off by default.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Manage {
    /// Allows `create_vm`.
    pub create: bool,
    /// Allows `delete_vm`.
    pub delete: bool,
    /// Allows changing vCPUs, memory, disks, CD-ROM media and network interfaces.
    pub reconfigure: bool,
    /// Allows creating, listing, reverting and deleting snapshots.
    pub snapshots: bool,
    /// Existing VMs (also in `allowed_vms`) the enabled operations may act on, in
    /// addition to the VMs this server created. `"*"` means every allowed VM.
    pub manage_vms: Vec<String>,
    /// libvirt storage pool where new disk images are created. Required for `create`.
    pub disk_pool: String,
    /// Host directories install / driver ISOs may be used from.
    pub iso_dirs: Vec<PathBuf>,
    /// libvirt networks VMs may be connected to.
    pub networks: Vec<String>,
    /// Max number of VMs created by this server that may exist at once.
    pub max_vms: u32,
    pub max_vcpus: u32,
    pub max_memory_mib: u64,
    /// Max size of a single disk.
    pub max_disk_gib: u64,
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
            share_project_dir: false,
            read_only: false,
            limits: Limits::default(),
            http: Http::default(),
            manage: Manage::default(),
        }
    }
}

impl Default for Manage {
    fn default() -> Self {
        Self {
            create: false,
            delete: false,
            reconfigure: false,
            snapshots: false,
            manage_vms: Vec::new(),
            disk_pool: String::new(),
            iso_dirs: Vec::new(),
            networks: Vec::new(),
            max_vms: 2,
            max_vcpus: 2,
            max_memory_mib: 4096,
            max_disk_gib: 32,
        }
    }
}

impl Manage {
    /// Whether any management operation is enabled.
    pub fn enabled(&self) -> bool {
        self.create || self.delete || self.reconfigure || self.snapshots
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

    /// Checks limits, resolves `host_share_dirs` to canonical paths and, with
    /// `share_project_dir`, adds the working directory to them.
    pub fn validate(&mut self) -> Result<()> {
        if self.limits.agent_timeout_secs <= 0 {
            bail!("limits.agent_timeout_secs must be positive");
        }
        if self.limits.console_buffer_bytes == 0 {
            bail!("limits.console_buffer_bytes must be positive");
        }
        canonicalize_dirs(&mut self.host_share_dirs, "host_share_dirs")?;
        canonicalize_dirs(&mut self.manage.iso_dirs, "manage.iso_dirs")?;
        let m = &self.manage;
        if m.create && m.disk_pool.is_empty() {
            bail!("manage.create needs manage.disk_pool");
        }
        if m.max_vcpus == 0 || m.max_memory_mib == 0 || m.max_disk_gib == 0 {
            bail!("manage.max_vcpus, max_memory_mib and max_disk_gib must be positive");
        }
        if self.share_project_dir {
            let cwd = std::env::current_dir()?.canonicalize()?;
            let home = std::env::var_os("HOME").and_then(|h| PathBuf::from(h).canonicalize().ok());
            match project_dir(cwd, home.as_deref()) {
                Ok(dir) => self.host_share_dirs.push(dir),
                Err(e) => tracing::warn!("share_project_dir: {e:#}"),
            }
        }
        Ok(())
    }

    pub fn is_allowed(&self, vm: &str) -> bool {
        self.allowed_vms.iter().any(|a| a == "*" || a == vm)
    }

    pub fn can_power(&self, vm: &str) -> bool {
        self.is_allowed(vm) && self.power_vms.iter().any(|a| a == "*" || a == vm)
    }

    /// Whether `vm` is listed in `manage.manage_vms` (VMs created by this server are
    /// recognised separately, from their libvirt metadata).
    pub fn in_manage_vms(&self, vm: &str) -> bool {
        self.is_allowed(vm) && self.manage.manage_vms.iter().any(|a| a == "*" || a == vm)
    }

    /// Resolves `path` (symlinks and `..` included) and checks that it is a regular
    /// file inside one of `host_share_dirs`.
    pub fn share_file(&self, path: &str) -> Result<PathBuf> {
        file_in(&self.host_share_dirs, path, "host_share_dirs")
    }

    /// Like `share_file`, for ISO images in `manage.iso_dirs`.
    pub fn iso_file(&self, path: &str) -> Result<PathBuf> {
        file_in(&self.manage.iso_dirs, path, "manage.iso_dirs")
    }
}

fn canonicalize_dirs(dirs: &mut [PathBuf], key: &str) -> Result<()> {
    for dir in dirs {
        let real = dir
            .canonicalize()
            .with_context(|| format!("{key}: cannot access {}", dir.display()))?;
        if !real.is_dir() {
            bail!("{key}: {} is not a directory", dir.display());
        }
        *dir = real;
    }
    Ok(())
}

fn file_in(dirs: &[PathBuf], path: &str, key: &str) -> Result<PathBuf> {
    let p = Path::new(path);
    if !p.is_absolute() {
        bail!("{path}: path must be absolute");
    }
    let real = p.canonicalize().with_context(|| format!("cannot access {path}"))?;
    if !dirs.iter().any(|d| real.starts_with(d)) {
        bail!("{path} is not inside {key}");
    }
    if !real.is_file() {
        bail!("{path} is not a regular file");
    }
    Ok(real)
}

/// Accepts `cwd` as a project directory unless sharing it would expose too much.
fn project_dir(cwd: PathBuf, home: Option<&Path>) -> Result<PathBuf> {
    if cwd.parent().is_none() || home.is_some_and(|h| h.starts_with(&cwd)) {
        bail!("not sharing {}: it is the root or home directory, not a project", cwd.display());
    }
    Ok(cwd)
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
        assert!(config.iso_file(&s(&share.join("sub/ok.txt"))).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn project_dir_refuses_root_and_home() {
        let home = Path::new("/home/u");
        assert!(project_dir("/home/u/code/app".into(), Some(home)).is_ok());
        assert!(project_dir("/home/u".into(), Some(home)).is_err());
        assert!(project_dir("/home".into(), Some(home)).is_err());
        assert!(project_dir("/".into(), Some(home)).is_err());
    }
}
