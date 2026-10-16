//! VM management: creating, deleting and reconfiguring VMs, and snapshots.
//!
//! VMs created here carry a metadata element in the `OWNER_NS` namespace. That
//! marker is what makes them accessible, power-controllable and manageable without
//! being listed in the config file.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use serde_json::{Value, json};
use virt::connect::Connect;
use virt::domain::Domain;
use virt::domain_snapshot::DomainSnapshot;
use virt::network::Network;
use virt::storage_pool::StoragePool;
use virt::storage_vol::StorageVol;
use virt::sys;

use crate::config::Manage;
use crate::hypervisor::{self, Hypervisor};

/// XML namespace of the metadata element that marks VMs created by this server.
const OWNER_NS: &str = "http://virtmanager-mcp/owner/1";

/// Serialises operations that check limits or pick names and then act on them.
static LOCK: Mutex<()> = Mutex::new(());

/// Whether `dom` was created by this server.
pub fn is_owned(dom: &Domain) -> bool {
    dom.get_metadata(sys::VIR_DOMAIN_METADATA_ELEMENT as i32, Some(OWNER_NS), 0).is_ok()
}

/// Parameters of a new VM, already validated against the config by `create`.
pub struct NewVm {
    pub name: String,
    pub windows: bool,
    pub vcpus: u32,
    pub memory_mib: u64,
    pub disk_gib: u64,
    pub uefi: bool,
    pub tpm: bool,
    pub iso: Option<String>,
    pub driver_iso: Option<String>,
    pub network: Option<String>,
}

pub fn create(hv: &Hypervisor, vm: NewVm) -> Result<String> {
    let m = &hv.config.manage;
    check_name(&vm.name, "VM name")?;
    check_vcpus(m, vm.vcpus)?;
    check_memory(m, vm.memory_mib)?;
    check_disk(m, vm.disk_gib)?;
    let iso = vm.iso.as_deref().map(|p| hv.config.iso_file(p)).transpose()?;
    let driver_iso = vm.driver_iso.as_deref().map(|p| hv.config.iso_file(p)).transpose()?;
    let network = match vm.network.clone() {
        Some(n) => Some(n),
        None => m.networks.first().cloned(),
    };

    let _guard = LOCK.lock().unwrap();
    hv.with_conn(|conn| {
        if let Some(n) = &network {
            check_network(conn, m, n)?;
        }
        if Domain::lookup_by_name(conn, &vm.name).is_ok() {
            bail!("a VM named '{}' already exists", vm.name);
        }
        let owned = conn.list_all_domains(0)?.iter().filter(|d| is_owned(d)).count();
        if owned >= m.max_vms as usize {
            bail!("this server already has {owned} VMs (manage.max_vms = {}); delete one first", m.max_vms);
        }
        let pool = disk_pool(conn, m)?;
        let vol_name = format!("{}.qcow2", vm.name);
        let vol = create_volume(&pool, &vol_name, vm.disk_gib)?;
        let disk = vol.get_path()?;
        let xml = domain_xml(&vm, &disk, iso.as_deref(), driver_iso.as_deref(), network.as_deref());
        if let Err(e) = Domain::define_xml(conn, &xml) {
            let _ = vol.delete(0);
            bail!("cannot define VM '{}': {}", vm.name, e.message());
        }
        let mut msg = format!(
            "created VM '{}' ({} vCPUs, {} MiB, {} GiB disk {}/{}, {}{}); it is shut off, start it with start_vm",
            vm.name,
            vm.vcpus,
            vm.memory_mib,
            vm.disk_gib,
            m.disk_pool,
            vol_name,
            if vm.uefi { "UEFI" } else { "BIOS" },
            if vm.tpm { ", TPM" } else { "" },
        );
        if let Some(n) = &network {
            msg.push_str(&format!("; network '{n}'"));
        }
        Ok(msg)
    })
}

/// Undefines a shut-off VM and deletes its disk volumes that are in the disk pool.
pub fn delete(hv: &Hypervisor, name: &str) -> Result<String> {
    let dom = hv.manage_domain(name)?;
    if dom.is_active()? {
        bail!("VM '{name}' is running; shut it down with stop_vm first");
    }
    let _guard = LOCK.lock().unwrap();
    let disks: Vec<DiskInfo> = parse_disks(&inactive_xml(&dom)?)?
        .into_iter()
        .filter(|d| d.device == "disk")
        .collect();
    hv.with_conn(|conn| {
        let m = &hv.config.manage;
        let pool = if m.disk_pool.is_empty() { None } else { Some(disk_pool(conn, m)?) };
        let mut remove = Vec::new();
        let mut kept = Vec::new();
        for d in &disks {
            match pool.as_ref().and_then(|p| pool_volume(conn, p, &m.disk_pool, d)) {
                Some(vol) => remove.push(vol),
                None => kept.push(d.source()),
            }
        }
        let flags = sys::VIR_DOMAIN_UNDEFINE_MANAGED_SAVE
            | sys::VIR_DOMAIN_UNDEFINE_SNAPSHOTS_METADATA
            | sys::VIR_DOMAIN_UNDEFINE_CHECKPOINTS_METADATA
            | sys::VIR_DOMAIN_UNDEFINE_NVRAM
            | sys::VIR_DOMAIN_UNDEFINE_TPM;
        dom.undefine_flags(flags)
            .map_err(|e| anyhow!("cannot undefine '{name}': {}", e.message()))?;

        let mut msg = format!("deleted VM '{name}'");
        for vol in remove {
            let path = vol.get_path().unwrap_or_default();
            match vol.delete(0) {
                Ok(()) => msg.push_str(&format!("; deleted disk {path}")),
                Err(e) => msg.push_str(&format!("; could not delete disk {path}: {}", e.message())),
            }
        }
        if !kept.is_empty() {
            msg.push_str(&format!("; left disks outside manage.disk_pool in place: {}", kept.join(", ")));
        }
        Ok(msg)
    })
}

/// Hardware summary of a VM's persistent configuration.
pub fn describe(hv: &Hypervisor, name: &str) -> Result<Value> {
    let dom = hv.manage_domain(name)?;
    let xml = inactive_xml(&dom)?;
    let doc = roxmltree::Document::parse(&xml)?;
    let root = doc.root_element();
    let text = |tag: &str| child(root, tag).and_then(|n| n.text()).unwrap_or_default().trim().to_string();
    let vcpu = child(root, "vcpu");
    let os = child(root, "os");
    let uefi = os.is_some_and(|o| o.attribute("firmware") == Some("efi") || child(o, "loader").is_some());
    let devices = child(root, "devices");
    let has = |tag: &str| devices.is_some_and(|d| child(d, tag).is_some());

    let m = &hv.config.manage;
    let disks: Vec<Value> = hv.with_conn(|conn| {
        let pool = if m.disk_pool.is_empty() { None } else { disk_pool(conn, m).ok() };
        Ok(parse_disks(&xml)?
            .iter()
            .map(|d| {
                let in_pool = d.device == "disk" && pool.as_ref().and_then(|p| pool_volume(conn, p, &m.disk_pool, d)).is_some();
                let mut v = json!({ "target": d.target, "device": d.device, "bus": d.bus, "source": d.source() });
                if d.device == "disk" {
                    v["in_disk_pool"] = json!(in_pool);
                }
                v
            })
            .collect())
    })?;
    Ok(json!({
        "name": name,
        "state": hypervisor::state_name(&dom),
        "created_by_this_server": is_owned(&dom),
        "vcpus": vcpu.and_then(|v| v.attribute("current")).map(str::to_string).unwrap_or_else(|| text("vcpu")),
        "max_vcpus": text("vcpu"),
        "memory_mib": kib_text(&xml, "currentMemory").or_else(|| kib_text(&xml, "memory")).map(|k| k / 1024),
        "firmware": if uefi { "uefi" } else { "bios" },
        "tpm": has("tpm"),
        "disks": disks,
        "interfaces": parse_nics(&xml)?,
        "note": "vcpus/memory are the persistent configuration and take effect at the next boot",
    }))
}

/// Changes vCPUs and/or memory in the persistent configuration (applies at next boot).
pub fn set_resources(hv: &Hypervisor, name: &str, vcpus: Option<u32>, memory_mib: Option<u64>) -> Result<String> {
    let m = &hv.config.manage;
    if vcpus.is_none() && memory_mib.is_none() {
        bail!("provide vcpus and/or memory_mib");
    }
    let dom = hv.manage_domain(name)?;
    let xml = inactive_xml(&dom)?;
    let doc = roxmltree::Document::parse(&xml)?;
    let cfg = sys::VIR_DOMAIN_AFFECT_CONFIG;
    let mut done = Vec::new();
    if let Some(n) = vcpus {
        check_vcpus(m, n)?;
        let max: u32 = child(doc.root_element(), "vcpu")
            .and_then(|v| v.text())
            .and_then(|t| t.trim().parse().ok())
            .context("cannot read the VM's vcpu count")?;
        // The current count may never exceed the maximum, so order the two calls.
        let steps = if n >= max {
            [cfg | sys::VIR_DOMAIN_VCPU_MAXIMUM, cfg]
        } else {
            [cfg, cfg | sys::VIR_DOMAIN_VCPU_MAXIMUM]
        };
        for flags in steps {
            dom.set_vcpus_flags(n, flags)
                .map_err(|e| anyhow!("cannot set vCPUs: {}", e.message()))?;
        }
        done.push(format!("{n} vCPUs"));
    }
    if let Some(mib) = memory_mib {
        check_memory(m, mib)?;
        let kib = mib * 1024;
        let max = kib_text(&xml, "memory").context("cannot read the VM's memory size")?;
        let steps = if kib >= max {
            [cfg | sys::VIR_DOMAIN_MEM_MAXIMUM, cfg]
        } else {
            [cfg, cfg | sys::VIR_DOMAIN_MEM_MAXIMUM]
        };
        for flags in steps {
            dom.set_memory_flags(kib, flags)
                .map_err(|e| anyhow!("cannot set memory: {}", e.message()))?;
        }
        done.push(format!("{mib} MiB memory"));
    }
    let mut msg = format!("VM '{name}' configured with {}", done.join(" and "));
    if dom.is_active()? {
        msg.push_str("; the VM is running, so this takes effect after it is shut down and started again");
    }
    Ok(msg)
}

/// Creates a new volume in the disk pool and attaches it as a virtio disk.
pub fn attach_disk(hv: &Hypervisor, name: &str, size_gib: u64) -> Result<String> {
    let m = &hv.config.manage;
    check_disk(m, size_gib)?;
    let dom = hv.manage_domain(name)?;
    let _guard = LOCK.lock().unwrap();
    let used: Vec<String> = parse_disks(&inactive_xml(&dom)?)?.into_iter().map(|d| d.target).collect();
    let target = ('b'..='z')
        .map(|c| format!("vd{c}"))
        .find(|t| !used.contains(t))
        .context("no free virtio disk target left")?;
    hv.with_conn(|conn| {
        let pool = disk_pool(conn, m)?;
        let vol_name = format!("{name}-{target}.qcow2");
        let vol = create_volume(&pool, &vol_name, size_gib)?;
        // A file path rather than a pool/volume reference, like virt-manager writes it:
        // some distributions' AppArmor setup does not label volume-type disks.
        let xml = format!(
            "<disk type='file' device='disk'><driver name='qemu' type='qcow2' discard='unmap'/>\
             <source file='{}'/><target dev='{target}' bus='virtio'/></disk>",
            esc(&vol.get_path()?)
        );
        if let Err(e) = dom.attach_device_flags(&xml, live_and_config(&dom)?) {
            let _ = vol.delete(0);
            bail!("cannot attach disk: {}{}", e.message(), running_hint(&dom));
        }
        Ok(format!("attached new {size_gib} GiB disk {}/{vol_name} to '{name}' as {target}", m.disk_pool))
    })
}

/// Detaches a disk from a shut-off VM and deletes its volume if it is in the disk pool.
pub fn detach_disk(hv: &Hypervisor, name: &str, target: &str) -> Result<String> {
    let m = &hv.config.manage;
    let dom = hv.manage_domain(name)?;
    if dom.is_active()? {
        bail!("VM '{name}' is running; shut it down with stop_vm first (the disk is deleted after detaching)");
    }
    let _guard = LOCK.lock().unwrap();
    let xml = inactive_xml(&dom)?;
    let disk = parse_disks(&xml)?
        .into_iter()
        .find(|d| d.target == target && d.device == "disk")
        .with_context(|| format!("VM '{name}' has no disk with target '{target}' (CD-ROMs are changed with set_cdrom)"))?;
    hv.with_conn(|conn| {
        let vol = if m.disk_pool.is_empty() {
            None
        } else {
            pool_volume(conn, &disk_pool(conn, m)?, &m.disk_pool, &disk)
        };
        dom.detach_device_flags(&xml[disk.range.clone()], sys::VIR_DOMAIN_AFFECT_CONFIG)
            .map_err(|e| anyhow!("cannot detach {target}: {}", e.message()))?;
        let mut msg = format!("detached {target} from '{name}'");
        match vol {
            Some(vol) => match vol.delete(0) {
                Ok(()) => msg.push_str(&format!("; deleted {}", disk.source())),
                Err(e) => msg.push_str(&format!("; could not delete {}: {}", disk.source(), e.message())),
            },
            None => msg.push_str(&format!("; {} is outside manage.disk_pool and was left in place", disk.source())),
        }
        Ok(msg)
    })
}

/// Inserts an ISO into a CD-ROM drive, or ejects it with `iso: None`. Without
/// `target` the first CD-ROM drive is used; a shut-off VM without one gets a new drive.
pub fn set_cdrom(hv: &Hypervisor, name: &str, target: Option<&str>, iso: Option<&str>) -> Result<String> {
    let iso = iso.map(|p| hv.config.iso_file(p)).transpose()?;
    let dom = hv.manage_domain(name)?;
    let _guard = LOCK.lock().unwrap();
    let active = dom.is_active()?;
    // The live definition carries the fields libvirt compares when changing media.
    let xml = if active { dom.get_xml_desc(0)? } else { inactive_xml(&dom)? };
    let disks = parse_disks(&xml)?;
    let source = iso.as_deref().map(|p| format!("<source file='{}'/>", esc(&p.to_string_lossy()))).unwrap_or_default();
    let what = match &iso {
        Some(p) => format!("inserted {}", p.display()),
        None => "ejected the medium".to_string(),
    };
    let cdrom = disks.iter().find(|d| d.device == "cdrom" && target.is_none_or(|t| d.target == t));
    let Some(cdrom) = cdrom else {
        if let Some(t) = target {
            bail!("VM '{name}' has no CD-ROM drive '{t}'");
        }
        if active {
            bail!("VM '{name}' has no CD-ROM drive; shut it down to add one");
        }
        let used: Vec<&str> = disks.iter().map(|d| d.target.as_str()).collect();
        let t = ('a'..='z')
            .map(|c| format!("sd{c}"))
            .find(|t| !used.contains(&t.as_str()))
            .context("no free SATA target left")?;
        let dev = format!(
            "<disk type='file' device='cdrom'><driver name='qemu' type='raw'/>{source}<target dev='{t}' bus='sata'/><readonly/></disk>"
        );
        dom.attach_device_flags(&dev, sys::VIR_DOMAIN_AFFECT_CONFIG)
            .map_err(|e| anyhow!("cannot add a CD-ROM drive: {}", e.message()))?;
        return Ok(format!("added CD-ROM drive {t} to '{name}' and {what}"));
    };

    // Keep the drive's definition and swap only its <source>.
    let el = &xml[cdrom.range.clone()];
    let doc = roxmltree::Document::parse(el)?;
    let root = doc.root_element();
    let body_start = el.find('>').context("malformed disk element")? + 1;
    // Replace the old <source> in place, or insert the new one before <target>.
    let cut = match (child(root, "source"), child(root, "target")) {
        (Some(s), _) => s.range(),
        (None, Some(t)) => t.range().start..t.range().start,
        (None, None) => body_start..body_start,
    };
    let dev = format!(
        "<disk type='file' device='cdrom'>{}{source}{}",
        &el[body_start..cut.start],
        &el[cut.end..]
    );
    dom.update_device_flags(&dev, live_and_config(&dom)?)
        .map_err(|e| anyhow!("cannot change media in {}: {}", cdrom.target, e.message()))?;
    Ok(format!("{what} in {} of '{name}'", cdrom.target))
}

pub fn attach_nic(hv: &Hypervisor, name: &str, network: &str) -> Result<String> {
    let m = &hv.config.manage;
    let dom = hv.manage_domain(name)?;
    hv.with_conn(|conn| check_network(conn, m, network))?;
    let before: Vec<String> = parse_nics(&inactive_xml(&dom)?)?.into_iter().map(|n| n.mac).collect();
    let xml = format!("<interface type='network'><source network='{}'/><model type='virtio'/></interface>", esc(network));
    dom.attach_device_flags(&xml, live_and_config(&dom)?)
        .map_err(|e| anyhow!("cannot attach network interface: {}{}", e.message(), running_hint(&dom)))?;
    let mac = parse_nics(&inactive_xml(&dom)?)?
        .into_iter()
        .map(|n| n.mac)
        .find(|mac| !before.contains(mac))
        .unwrap_or_default();
    Ok(format!("attached a network interface on '{network}' to '{name}' (MAC {mac})"))
}

pub fn detach_nic(hv: &Hypervisor, name: &str, mac: &str) -> Result<String> {
    let dom = hv.manage_domain(name)?;
    let xml = inactive_xml(&dom)?;
    let nic = parse_nics(&xml)?
        .into_iter()
        .find(|n| n.mac.eq_ignore_ascii_case(mac))
        .with_context(|| format!("VM '{name}' has no network interface with MAC {mac}"))?;
    dom.detach_device_flags(&xml[nic.range.clone()], live_and_config(&dom)?)
        .map_err(|e| anyhow!("cannot detach network interface: {}", e.message()))?;
    Ok(format!("detached network interface {} from '{name}'", nic.mac))
}

pub fn snapshot_create(hv: &Hypervisor, name: &str, snapshot: &str, description: Option<&str>) -> Result<String> {
    check_name(snapshot, "snapshot name")?;
    let dom = hv.manage_domain(name)?;
    let xml = format!(
        "<domainsnapshot><name>{}</name><description>{}</description></domainsnapshot>",
        esc(snapshot),
        esc(description.unwrap_or_default())
    );
    DomainSnapshot::create_xml(&dom, &xml, 0).map_err(|e| {
        let mut msg = format!("cannot create snapshot '{snapshot}': {}", e.message());
        if e.message().contains("pflash") {
            msg.push_str(" (a running UEFI VM cannot be snapshotted here; shut it down with stop_vm first)");
        }
        anyhow!(msg)
    })?;
    Ok(format!("created snapshot '{snapshot}' of '{name}' ({})", hypervisor::state_name(&dom)))
}

pub fn snapshot_list(hv: &Hypervisor, name: &str) -> Result<Vec<Value>> {
    let dom = hv.manage_domain(name)?;
    let mut out = Vec::new();
    for snap in dom.list_all_snapshots(0)? {
        let xml = snap.get_xml_desc(0)?;
        let doc = roxmltree::Document::parse(&xml)?;
        let root = doc.root_element();
        let text = |tag: &str| child(root, tag).and_then(|n| n.text()).map(str::to_string);
        out.push(json!({
            "name": text("name"),
            "description": text("description"),
            "vm_state": text("state"),
            "created": text("creationTime").and_then(|t| t.parse::<i64>().ok()),
            "parent": child(root, "parent").and_then(|p| child(p, "name")).and_then(|n| n.text()),
            "current": snap.is_current(0).unwrap_or(false),
        }));
    }
    out.sort_by_key(|v| v["created"].as_i64());
    Ok(out)
}

pub fn snapshot_revert(hv: &Hypervisor, name: &str, snapshot: &str) -> Result<String> {
    let dom = hv.manage_domain(name)?;
    let snap = DomainSnapshot::lookup_by_name(&dom, snapshot, 0)
        .map_err(|e| anyhow!("snapshot '{snapshot}' not found: {}", e.message()))?;
    snap.revert(0)
        .map_err(|e| anyhow!("cannot revert to '{snapshot}': {}", e.message()))?;
    Ok(format!("reverted '{name}' to snapshot '{snapshot}'; the VM is now {}", hypervisor::state_name(&dom)))
}

pub fn snapshot_delete(hv: &Hypervisor, name: &str, snapshot: &str) -> Result<String> {
    let dom = hv.manage_domain(name)?;
    let snap = DomainSnapshot::lookup_by_name(&dom, snapshot, 0)
        .map_err(|e| anyhow!("snapshot '{snapshot}' not found: {}", e.message()))?;
    snap.delete(0)
        .map_err(|e| anyhow!("cannot delete snapshot '{snapshot}': {}", e.message()))?;
    Ok(format!("deleted snapshot '{snapshot}' of '{name}'"))
}

#[derive(Serialize)]
pub struct IsoFile {
    path: String,
    size_bytes: u64,
}

/// `.iso` files in `manage.iso_dirs` (searched up to three directory levels deep).
pub fn list_isos(m: &Manage) -> Vec<IsoFile> {
    fn walk(dir: &Path, depth: u32, out: &mut Vec<IsoFile>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::metadata(&path) else { continue };
            if meta.is_dir() && depth < 3 {
                walk(&path, depth + 1, out);
            } else if meta.is_file() && path.extension().is_some_and(|e| e.eq_ignore_ascii_case("iso")) {
                out.push(IsoFile { path: path.to_string_lossy().into_owned(), size_bytes: meta.len() });
            }
        }
    }
    let mut out = Vec::new();
    for dir in &m.iso_dirs {
        walk(dir, 0, &mut out);
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

// ---- helpers ------------------------------------------------------------------

fn check_name(name: &str, what: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !name.starts_with(['.', '-']);
    if !ok {
        bail!("{what} must be 1-64 characters of A-Z, a-z, 0-9, '.', '_', '-' and not start with '.' or '-'");
    }
    Ok(())
}

fn check_vcpus(m: &Manage, n: u32) -> Result<()> {
    if n == 0 || n > m.max_vcpus {
        bail!("vcpus must be between 1 and {} (manage.max_vcpus)", m.max_vcpus);
    }
    Ok(())
}

fn check_memory(m: &Manage, mib: u64) -> Result<()> {
    if !(256..=m.max_memory_mib).contains(&mib) {
        bail!("memory_mib must be between 256 and {} (manage.max_memory_mib)", m.max_memory_mib);
    }
    Ok(())
}

fn check_disk(m: &Manage, gib: u64) -> Result<()> {
    if gib == 0 || gib > m.max_disk_gib {
        bail!("disk size must be between 1 and {} GiB (manage.max_disk_gib)", m.max_disk_gib);
    }
    Ok(())
}

fn check_network(conn: &Connect, m: &Manage, network: &str) -> Result<()> {
    if !m.networks.iter().any(|n| n == network) {
        bail!("network '{network}' is not in manage.networks ({})", m.networks.join(", "));
    }
    Network::lookup_by_name(conn, network).map_err(|e| anyhow!("network '{network}': {}", e.message()))?;
    Ok(())
}

fn disk_pool(conn: &Connect, m: &Manage) -> Result<StoragePool> {
    if m.disk_pool.is_empty() {
        bail!("manage.disk_pool is not set");
    }
    let pool = StoragePool::lookup_by_name(conn, &m.disk_pool)
        .map_err(|e| anyhow!("storage pool '{}': {}", m.disk_pool, e.message()))?;
    if !pool.is_active()? {
        bail!("storage pool '{}' is not active", m.disk_pool);
    }
    let _ = pool.refresh(0);
    Ok(pool)
}

fn create_volume(pool: &StoragePool, name: &str, gib: u64) -> Result<StorageVol> {
    if StorageVol::lookup_by_name(pool, name).is_ok() {
        bail!("volume '{name}' already exists in the disk pool");
    }
    let xml = format!(
        "<volume><name>{}</name><capacity unit='GiB'>{gib}</capacity><allocation>0</allocation>\
         <target><format type='qcow2'/></target></volume>",
        esc(name)
    );
    StorageVol::create_xml(pool, &xml, 0).map_err(|e| anyhow!("cannot create volume '{name}': {}", e.message()))
}

/// The pool volume backing `disk`, if it lives in the pool named `pool_name`.
fn pool_volume(conn: &Connect, pool: &StoragePool, pool_name: &str, disk: &DiskInfo) -> Option<StorageVol> {
    if let (Some(p), Some(v)) = (&disk.pool, &disk.volume) {
        return if p == pool_name { StorageVol::lookup_by_name(pool, v).ok() } else { None };
    }
    let vol = StorageVol::lookup_by_path(conn, disk.file.as_deref()?).ok()?;
    let owner = StoragePool::lookup_by_volume(&vol).ok()?.get_name().ok()?;
    (owner == pool_name).then_some(vol)
}

/// Device changes go to the persistent config, and to the live VM too if it is running.
fn live_and_config(dom: &Domain) -> Result<u32> {
    let mut flags = sys::VIR_DOMAIN_AFFECT_CONFIG;
    if dom.is_active()? {
        flags |= sys::VIR_DOMAIN_AFFECT_LIVE;
    }
    Ok(flags)
}

/// Suffix for hot-plug errors: the same change usually works on a shut-off VM.
fn running_hint(dom: &Domain) -> &'static str {
    if dom.is_active().unwrap_or(false) {
        " (the VM is running; shutting it down with stop_vm and retrying may work)"
    } else {
        ""
    }
}

fn inactive_xml(dom: &Domain) -> Result<String> {
    Ok(dom.get_xml_desc(sys::VIR_DOMAIN_XML_INACTIVE)?)
}

fn child<'a, 'i>(node: roxmltree::Node<'a, 'i>, tag: &str) -> Option<roxmltree::Node<'a, 'i>> {
    node.children().find(|n| n.has_tag_name(tag))
}

/// Value of a top-level memory element (`memory`, `currentMemory`) in KiB.
fn kib_text(xml: &str, tag: &str) -> Option<u64> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let n = child(doc.root_element(), tag)?;
    let v: u64 = n.text()?.trim().parse().ok()?;
    let factor = match n.attribute("unit").unwrap_or("KiB") {
        "b" | "bytes" => return Some(v / 1024),
        "KiB" | "k" => 1,
        "MiB" | "M" => 1024,
        "GiB" | "G" => 1024 * 1024,
        _ => return None,
    };
    Some(v * factor)
}

struct DiskInfo {
    device: String,
    target: String,
    bus: String,
    file: Option<String>,
    pool: Option<String>,
    volume: Option<String>,
    /// Byte range of the `<disk>` element in the XML it was parsed from.
    range: std::ops::Range<usize>,
}

impl DiskInfo {
    fn source(&self) -> String {
        match (&self.file, &self.pool, &self.volume) {
            (Some(f), _, _) => f.clone(),
            (None, Some(p), Some(v)) => format!("{p}/{v}"),
            _ => "(empty)".into(),
        }
    }
}

fn parse_disks(xml: &str) -> Result<Vec<DiskInfo>> {
    let doc = roxmltree::Document::parse(xml)?;
    let Some(devices) = child(doc.root_element(), "devices") else { return Ok(Vec::new()) };
    Ok(devices
        .children()
        .filter(|n| n.has_tag_name("disk"))
        .map(|d| {
            let source = child(d, "source");
            let src = |a: &str| source.and_then(|s| s.attribute(a)).map(str::to_string);
            let target = child(d, "target");
            DiskInfo {
                device: d.attribute("device").unwrap_or("disk").to_string(),
                target: target.and_then(|t| t.attribute("dev")).unwrap_or_default().to_string(),
                bus: target.and_then(|t| t.attribute("bus")).unwrap_or_default().to_string(),
                file: src("file").or_else(|| src("dev")),
                pool: src("pool"),
                volume: src("volume"),
                range: d.range(),
            }
        })
        .collect())
}

#[derive(Serialize)]
struct NicInfo {
    mac: String,
    #[serde(rename = "type")]
    kind: String,
    source: String,
    model: String,
    #[serde(skip)]
    range: std::ops::Range<usize>,
}

fn parse_nics(xml: &str) -> Result<Vec<NicInfo>> {
    let doc = roxmltree::Document::parse(xml)?;
    let Some(devices) = child(doc.root_element(), "devices") else { return Ok(Vec::new()) };
    Ok(devices
        .children()
        .filter(|n| n.has_tag_name("interface"))
        .map(|i| {
            let attr = |tag: &str, a: &str| child(i, tag).and_then(|n| n.attribute(a)).unwrap_or_default().to_string();
            let source = child(i, "source")
                .and_then(|s| s.attribute("network").or(s.attribute("bridge")).or(s.attribute("dev")))
                .unwrap_or_default()
                .to_string();
            NicInfo {
                mac: attr("mac", "address"),
                kind: i.attribute("type").unwrap_or_default().to_string(),
                source,
                model: attr("model", "type"),
                range: i.range(),
            }
        })
        .collect())
}

/// Escapes text for use in XML content and single- or double-quoted attributes.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\'', "&apos;")
        .replace('"', "&quot;")
}

fn domain_xml(
    vm: &NewVm,
    disk: &str,
    iso: Option<&Path>,
    driver_iso: Option<&Path>,
    network: Option<&str>,
) -> String {
    let cdrom = |dev: &str, iso: Option<&Path>, boot: &str| {
        let source = iso.map(|p| format!("<source file='{}'/>", esc(&p.to_string_lossy()))).unwrap_or_default();
        format!(
            "<disk type='file' device='cdrom'><driver name='qemu' type='raw'/>{source}\
             <target dev='{dev}' bus='sata'/><readonly/>{boot}</disk>"
        )
    };
    // Root ports for every PCIe device plus spares, so disks and NICs can be hot-plugged.
    let mut devices = vec!["<controller type='pci' index='0' model='pcie-root'/>".to_string()];
    devices.extend(std::iter::repeat_n("<controller type='pci' model='pcie-root-port'/>".to_string(), 14));
    devices.extend([
        format!(
            "<disk type='file' device='disk'><driver name='qemu' type='qcow2' discard='unmap'/>\
             <source file='{}'/><target dev='vda' bus='virtio'/><boot order='1'/></disk>",
            esc(disk)
        ),
        cdrom("sda", iso, "<boot order='2'/>"),
    ]);
    if driver_iso.is_some() {
        devices.push(cdrom("sdb", driver_iso, ""));
    }
    if let Some(n) = network {
        devices.push(format!("<interface type='network'><source network='{}'/><model type='virtio'/></interface>", esc(n)));
    }
    devices.extend(
        [
            "<controller type='usb' model='qemu-xhci'/>",
            "<input type='tablet' bus='usb'/>",
            "<channel type='unix'><target type='virtio' name='org.qemu.guest_agent.0'/></channel>",
            "<serial type='pty'><target type='isa-serial'/></serial>",
            "<console type='pty'><target type='serial'/></console>",
            "<graphics type='spice' autoport='yes'><listen type='address'/><image compression='off'/></graphics>",
            "<video><model type='virtio'/></video>",
            "<memballoon model='virtio'/>",
            "<rng model='virtio'><backend model='random'>/dev/urandom</backend></rng>",
        ]
        .map(String::from),
    );
    if vm.tpm {
        devices.push("<tpm model='tpm-crb'><backend type='emulator' version='2.0'/></tpm>".into());
    }
    let features = if vm.windows {
        "<acpi/><apic/><hyperv mode='custom'><relaxed state='on'/><vapic state='on'/>\
         <spinlocks state='on' retries='8191'/><vpindex state='on'/><synic state='on'/>\
         <stimer state='on'/><frequencies state='on'/></hyperv>"
    } else {
        "<acpi/><apic/>"
    };
    let clock = if vm.windows {
        "<clock offset='localtime'><timer name='rtc' tickpolicy='catchup'/><timer name='pit' tickpolicy='delay'/>\
         <timer name='hpet' present='no'/><timer name='hypervclock' present='yes'/></clock>"
    } else {
        "<clock offset='utc'><timer name='rtc' tickpolicy='catchup'/><timer name='pit' tickpolicy='delay'/>\
         <timer name='hpet' present='no'/></clock>"
    };
    format!(
        "<domain type='kvm'>\
         <name>{name}</name>\
         <metadata><vmm:owner xmlns:vmm='{OWNER_NS}'/></metadata>\
         <memory unit='MiB'>{mem}</memory><currentMemory unit='MiB'>{mem}</currentMemory>\
         <vcpu>{vcpus}</vcpu>\
         <os{firmware}><type arch='x86_64' machine='q35'>hvm</type></os>\
         <features>{features}</features>\
         <cpu mode='host-passthrough'/>\
         {clock}\
         <pm><suspend-to-mem enabled='no'/><suspend-to-disk enabled='no'/></pm>\
         <devices>{devices}</devices>\
         </domain>",
        name = esc(&vm.name),
        mem = vm.memory_mib,
        vcpus = vm.vcpus,
        firmware = if vm.uefi { " firmware='efi'" } else { "" },
        devices = devices.concat(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(check_name("test-vm_1.2", "n").is_ok());
        for bad in ["", "-x", ".x", "a b", "a/b", "a'b", &"x".repeat(65)] {
            assert!(check_name(bad, "n").is_err(), "{bad}");
        }
    }

    #[test]
    fn domain_xml_parses_and_escapes() {
        let vm = NewVm {
            name: "t".into(),
            windows: true,
            vcpus: 2,
            memory_mib: 2048,
            disk_gib: 10,
            uefi: true,
            tpm: true,
            iso: None,
            driver_iso: None,
            network: Some("default".into()),
        };
        let xml = domain_xml(&vm, "/pool/t.qcow2", Some(Path::new("/isos/a'b&c.iso")), None, Some("net"));
        let doc = roxmltree::Document::parse(&xml).unwrap();
        let disks = parse_disks(&xml).unwrap();
        assert_eq!(disks[1].file.as_deref(), Some("/isos/a'b&c.iso"));
        assert_eq!(disks[0].file.as_deref(), Some("/pool/t.qcow2"));
        assert!(doc.descendants().any(|n| n.has_tag_name("tpm")));
        assert_eq!(kib_text(&xml, "memory"), Some(2048 * 1024));
    }
}
