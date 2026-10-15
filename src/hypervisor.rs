//! libvirt connection handling and access control.

use std::sync::RwLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use virt::connect::Connect;
use virt::domain::Domain;
use virt::sys;

use crate::config::Config;

/// Starts libvirt's default event loop on a background thread. Must run before
/// the connection is opened: the remote driver only delivers stream data
/// (serial console) and keepalives when an event loop is registered.
pub fn start_event_loop() -> Result<()> {
    virt::event::event_register_default_impl().context("registering libvirt event loop")?;
    std::thread::Builder::new()
        .name("libvirt-events".into())
        .spawn(|| {
            loop {
                if let Err(e) = virt::event::event_run_default_impl() {
                    tracing::error!("libvirt event loop: {e}");
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        })?;
    Ok(())
}

pub struct Hypervisor {
    pub config: Config,
    conn: RwLock<Connect>,
}

impl Hypervisor {
    pub fn connect(config: Config) -> Result<Self> {
        let conn = open(&config.uri)?;
        Ok(Self {
            config,
            conn: RwLock::new(conn),
        })
    }

    /// Runs `f` with a live connection, reconnecting once if libvirtd went away.
    pub fn with_conn<T>(&self, f: impl FnOnce(&Connect) -> Result<T>) -> Result<T> {
        {
            let conn = self.conn.read().unwrap();
            if conn.is_alive().unwrap_or(false) {
                return f(&conn);
            }
        }
        let mut conn = self.conn.write().unwrap();
        if !conn.is_alive().unwrap_or(false) {
            tracing::warn!("libvirt connection lost, reconnecting to {}", self.config.uri);
            let _ = conn.close();
            *conn = open(&self.config.uri)?;
        }
        f(&conn)
    }

    /// Looks up a VM by name, enforcing the allow-list.
    pub fn lookup(&self, name: &str) -> Result<Domain> {
        if !self.config.is_allowed(name) {
            bail!("VM '{name}' is not in allowed_vms");
        }
        self.with_conn(|conn| {
            Domain::lookup_by_name(conn, name).map_err(|e| anyhow!("VM '{name}' not found: {}", e.message()))
        })
    }

    /// Looks up a VM by name, enforcing the allow-list and that it is running.
    pub fn domain(&self, name: &str) -> Result<Domain> {
        let dom = self.lookup(name)?;
        if !dom.is_active()? {
            if self.config.can_power(name) {
                bail!("VM '{name}' is not running (start it with start_vm)");
            }
            bail!("VM '{name}' is not running (this server may not start it)");
        }
        Ok(dom)
    }

    /// All VMs (running or not) that are in the allow-list.
    pub fn allowed(&self) -> Result<Vec<Domain>> {
        self.with_conn(|conn| {
            let doms = conn.list_all_domains(0)?;
            Ok(doms
                .into_iter()
                .filter(|d| d.get_name().is_ok_and(|n| self.config.is_allowed(&n)))
                .collect())
        })
    }

    /// Looks up a VM the AI may power on/off.
    fn power_domain(&self, name: &str) -> Result<Domain> {
        if self.config.is_allowed(name) && !self.config.can_power(name) {
            bail!("VM '{name}' is not in power_vms");
        }
        self.lookup(name)
    }

    /// Boots, resumes or wakes a VM. Returns a short description of what happened.
    pub fn start(&self, name: &str) -> Result<String> {
        let dom = self.power_domain(name)?;
        if !dom.is_active()? {
            dom.create().map_err(|e| anyhow!("cannot start '{name}': {}", e.message()))?;
            return Ok("started".into());
        }
        Ok(match dom.get_state()?.0 {
            sys::VIR_DOMAIN_PAUSED => {
                dom.resume().map_err(|e| anyhow!("cannot resume '{name}': {}", e.message()))?;
                "resumed from pause".into()
            }
            sys::VIR_DOMAIN_PMSUSPENDED => {
                dom.pm_wakeup(0).map_err(|e| anyhow!("cannot wake '{name}': {}", e.message()))?;
                "woken from suspend".into()
            }
            sys::VIR_DOMAIN_CRASHED => bail!("VM '{name}' has crashed; stop it with force: true, then start it"),
            _ => "already running".into(),
        })
    }

    /// Requests a guest shutdown (ACPI or guest agent, whichever libvirt picks), or
    /// powers the VM off immediately with `force`. Waits up to `wait` for it to stop.
    /// Returns whether the VM is off.
    pub fn stop(&self, name: &str, force: bool, wait: Duration) -> Result<bool> {
        let dom = self.power_domain(name)?;
        if !dom.is_active()? {
            return Ok(true);
        }
        if force {
            dom.destroy().map_err(|e| anyhow!("cannot power off '{name}': {}", e.message()))?;
            return Ok(true);
        }
        dom.shutdown().map_err(|e| anyhow!("cannot shut down '{name}': {}", e.message()))?;
        let start = Instant::now();
        while dom.is_active()? {
            if start.elapsed() >= wait {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        Ok(true)
    }
}

/// libvirt's state name for a VM, e.g. "running" or "shutoff".
pub fn state_name(dom: &Domain) -> &'static str {
    match dom.get_state().map(|s| s.0) {
        Ok(sys::VIR_DOMAIN_RUNNING) => "running",
        Ok(sys::VIR_DOMAIN_BLOCKED) => "blocked",
        Ok(sys::VIR_DOMAIN_PAUSED) => "paused",
        Ok(sys::VIR_DOMAIN_SHUTDOWN) => "shutting down",
        Ok(sys::VIR_DOMAIN_SHUTOFF) => "shutoff",
        Ok(sys::VIR_DOMAIN_CRASHED) => "crashed",
        Ok(sys::VIR_DOMAIN_PMSUSPENDED) => "suspended",
        _ => "unknown",
    }
}

fn open(uri: &str) -> Result<Connect> {
    Connect::open(Some(uri)).map_err(|e| {
        anyhow!(
            "cannot connect to {uri}: {} (for qemu:///system your user usually needs to be in the 'libvirt' group)",
            e.message()
        )
    })
}
