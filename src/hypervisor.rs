//! libvirt connection handling and access control.

use std::sync::RwLock;

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

    /// Looks up a VM by name, enforcing the allow-list and that it is running.
    pub fn domain(&self, name: &str) -> Result<Domain> {
        if !self.config.is_allowed(name) {
            bail!("VM '{name}' is not in allowed_vms");
        }
        self.with_conn(|conn| {
            let dom = Domain::lookup_by_name(conn, name)
                .map_err(|e| anyhow!("VM '{name}' not found: {}", e.message()))?;
            if !dom.is_active()? {
                bail!("VM '{name}' is not running (this server does not start VMs)");
            }
            Ok(dom)
        })
    }

    /// Names of running VMs that are in the allow-list.
    pub fn running_allowed(&self) -> Result<Vec<Domain>> {
        self.with_conn(|conn| {
            let doms = conn.list_all_domains(sys::VIR_CONNECT_LIST_DOMAINS_ACTIVE)?;
            Ok(doms
                .into_iter()
                .filter(|d| d.get_name().is_ok_and(|n| self.config.is_allowed(&n)))
                .collect())
        })
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
