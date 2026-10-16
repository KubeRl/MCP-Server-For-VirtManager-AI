The commits are the only non-vibecoded part of this project, and also this line. Rust is hard man. And I'm too lazy to learn.

# virtmanager-mcp

An MCP server that gives an AI access to libvirt / virt-manager VMs:
command execution, file access, copying host files into guests, the serial console, screenshots
and keyboard input. VMs you opt in can also be started and stopped. Creating, deleting and
reconfiguring VMs and snapshots are available too, but are off until you enable them (see
[VM management](#vm-management)).

Host: Linux. Guests: Linux and Windows.

## How it talks to the guest

| Capability | Mechanism | Guest requirement |
|---|---|---|
| `exec`, `read_file`, `write_file`, `copy_to_vm`, `list_dir`, `stat_path`, `guest_info` | QEMU guest agent over a virtio-serial channel (`virDomainQemuAgentCommand`) | qemu-guest-agent running |
| `console_open/read/write/close` | Serial / virtio console stream (`virDomainOpenConsole`, like `virsh console`) | `<console>`/`<serial>` device + getty on it |
| `screenshot`, `send_keys`, `type_text` | Graphical display (`virDomainScreenshot`, `virDomainSendKey`) | A graphics device (SPICE/VNC) |
| `start_vm`, `stop_vm` | libvirt (`virDomainCreate`, `virDomainShutdown`, `virDomainDestroy`) | none (graceful stop needs ACPI or the guest agent) |
| VM management tools | libvirt domain, storage volume and snapshot APIs | none |

`list_dir` and `stat_path` run fixed helper commands through `guest-exec`
(`stat`/`find` on Linux, PowerShell on Windows), because qemu-ga has no directory listing.
The path is passed as data, never spliced into shell code.

## Tools

| Tool | Read-only mode |
|---|---|
| `list_vms`: allowed VMs, power state, agent status, OS family | ✅ |
| `read_file`, `list_dir`, `stat_path` | ✅ |
| `console_open`, `console_read`, `console_close` | ✅ |
| `exec`: shell command (`sh`/`bash`/`cmd`/`powershell`) or program + args, with timeout | ❌ |
| `write_file` (overwrite/append, text or base64) | ❌ |
| `copy_to_vm`: copy a host file into the guest (only with `host_share_dirs`) | ❌ |
| `start_vm`, `stop_vm` (graceful or `force`) (only for VMs in `power_vms`) | ❌ |
| `console_write` | ❌ |
| `guest_info`: OS, hostname, NICs, filesystems, users, timezone | ❌ |
| `screenshot` (PNG, downscaled to 1280 px wide by default) | ❌ |
| `send_keys` (`ctrl+alt+delete`, `win+r`, …), `type_text` (US layout) | ❌ |
| `create_vm`, `delete_vm` (only with `manage.create` / `manage.delete`) | ❌ |
| `configure_vm`, `attach_disk`, `detach_disk`, `set_cdrom`, `attach_nic`, `detach_nic` (only with `manage.reconfigure`) | ❌ |
| `snapshot_create`, `snapshot_list`, `snapshot_revert`, `snapshot_delete` (only with `manage.snapshots`) | ❌ |
| `describe_vm` (with any `manage` operation), `list_isos` (with create/reconfigure and `iso_dirs`) | ❌ |

In read-only mode the ❌ tools are not advertised at all. `start_vm`/`stop_vm` are also hidden while
`power_vms` is empty (unless VM management is on), and `copy_to_vm` while no host directory is shared
(`host_share_dirs` / `share_project_dir`).

## Build

Requires the libvirt development files (`libvirt` on Arch, `libvirt-dev` on Debian/Ubuntu,
`libvirt-devel` on Fedora).

```sh
cargo build --release
# binary: target/release/virtmanager-mcp
```
Obviously, you don't have to build it if you download the release file.

## Host setup

1. Access to `qemu:///system`: add your user to the `libvirt` group and log in again.
   ```sh
   sudo usermod -aG libvirt $USER
   ```
2. Configuration: copy `config.example.toml` to `~/.config/virtmanager-mcp/config.toml` and list
   your VMs in `allowed_vms`. **No VM is accessible until it is listed** (or `"*"` is used).
   CLI flags override the file: `--uri`, `--allow <VM>` (repeatable), `--allow-power <VM>`
   (repeatable), `--share <DIR>` (repeatable), `--share-project-dir`, `--read-only`, `--bind`,
   `--config`.
3. Optional: list VMs the AI may start and stop in `power_vms` (they must also be in `allowed_vms`),
   and host directories it may copy files from in `host_share_dirs`. To allow copying only from the
   project you're working in, set `share_project_dir = true` instead: Claude Code starts the
   server in the session's project directory, and that directory becomes the only share. It is
   ignored when that directory is `/` or your home directory. In HTTP mode there's one shared
   server, so the "project" is wherever you started it.

## VM management

All of it is off by default. Enable each operation separately in the `[manage]` section of the
config file (see `config.example.toml`):

```toml
[manage]
create = true
delete = true
reconfigure = true
snapshots = true
manage_vms = []                  # existing VMs (also in allowed_vms) the tools may act on
disk_pool = "default"            # libvirt pool for new disks, required for create
iso_dirs = ["/var/lib/libvirt/iso"]
networks = ["default"]
max_vms = 2
max_vcpus = 2
max_memory_mib = 4096
max_disk_gib = 32
```

- **Which VMs:** the management tools act on VMs this server created and on VMs listed in
  `manage_vms`. A VM created by the server carries a marker in its libvirt metadata. While any
  `manage` operation is enabled, such VMs are also treated as if they were in `allowed_vms` and
  `power_vms`, so the AI can start them, use `exec` and so on. Other VMs stay off limits.
  `list_vms` shows `managed: true` for VMs the tools may act on.
- **Creating:** `create_vm` takes a name, `os` (`linux`/`windows`), vCPUs, memory, disk size,
  `firmware` (`uefi`, the default, or `bios`), an optional TPM 2.0, an install ISO, a second
  "driver" ISO (e.g. `virtio-win.iso` for Windows) and a network. The VM gets a q35 machine,
  a virtio disk (a new qcow2 volume `<name>.qcow2` in `disk_pool`), a SATA CD-ROM it boots from
  after the disk, a virtio NIC, the guest agent channel, a serial console, SPICE and spare PCIe
  ports for hot-plugging. Windows guests also get Hyper-V enlightenments and a local-time clock.
  The VM is created shut off.
- **Deleting:** `delete_vm` needs the VM to be shut off. It removes the VM with its snapshots,
  NVRAM and TPM state, and deletes the VM's disks that are volumes in `disk_pool`. Disks
  elsewhere and ISOs are never deleted. The same applies to VMs in `manage_vms`.
- **Reconfiguring:** `configure_vm` changes vCPUs and memory in the persistent configuration
  (takes effect at the next boot). `attach_disk` creates a new volume in `disk_pool` and attaches
  it, also while the VM is running. `detach_disk` needs the VM shut off and **deletes** the volume
  if it is in `disk_pool`. `set_cdrom` inserts or ejects ISOs, `attach_nic`/`detach_nic` add and
  remove interfaces on allowed networks. `describe_vm` shows targets and MAC addresses.
- **Limits:** `max_vcpus`, `max_memory_mib` and `max_disk_gib` apply to create and reconfigure.
  `max_vms` caps how many VMs created by the server exist at once.
- **Paths:** ISOs must be inside `iso_dirs` (resolved like `host_share_dirs`, symlinks
  included), and QEMU (user `libvirt-qemu`/`qemu`) must be able to read them. `list_isos` lists the
  `.iso` files found there.
- **Snapshots** are libvirt's internal qcow2 snapshots. A running VM's snapshot includes its
  memory. A *running* UEFI VM cannot be snapshotted on hosts whose OVMF firmware has no qcow2
  NVRAM variant (Arch, Ubuntu): libvirt refuses, so shut it down first. BIOS VMs work either way.

## Guest setup

### Guest agent channel (both OSes)

The VM needs a guest agent channel. virt-manager adds one automatically for new VMs; check the XML
(*Details → Overview → XML*) for:

```xml
<channel type="unix">
  <target type="virtio" name="org.qemu.guest_agent.0"/>
</channel>
```

If it is missing, add it (*Add Hardware → Channel → org.qemu.guest_agent.0*) and power-cycle the VM.

### Linux guest

```sh
sudo dnf install qemu-guest-agent     # or apt / pacman
sudo systemctl enable --now qemu-guest-agent
```

> **RHEL / CentOS / Rocky / Alma:** the packaged agent blocks `guest-exec` and file
> operations by default. Remove them from `BLOCK_RPCS` (or the `--block-rpcs` / `FILTER_RPC_ARGS`
> setting) in `/etc/sysconfig/qemu-ga` and restart the agent.

Serial console (for `console_*`): the default virt-manager VM has a `pty` serial console. Enable a login
on it with:

```sh
sudo systemctl enable --now serial-getty@ttyS0.service
```

To also see kernel/boot messages there, add `console=ttyS0` to the kernel command line.

### Windows guest

1. Install the [virtio-win drivers](https://github.com/virtio-win/virtio-win-pkg-scripts)
   (`virtio-win-guest-tools.exe`). It installs the VirtIO serial driver and the **QEMU Guest Agent** service.
2. Make sure the *QEMU Guest Agent* service is running (`services.msc`).

`exec` defaults to PowerShell on Windows (`shell: "cmd"` is also available). `list_dir`/`stat_path`
need PowerShell. Windows does not offer a serial console login by default, so use `exec`
or `screenshot`/`send_keys`/`type_text` instead of `console_*`.

## Connecting an MCP client

### stdio (default)

Claude Code:

```sh
claude mcp add virtmanager -- /path/to/virtmanager-mcp
```

Generic JSON config (Claude Desktop and others):

```json
{
  "mcpServers": {
    "virtmanager": { "command": "/path/to/virtmanager-mcp", "args": [] }
  }
}
```

### Streamable HTTP

```sh
VIRTMANAGER_MCP_TOKEN=change-me virtmanager-mcp --transport http   # http://127.0.0.1:8765/mcp
claude mcp add --transport http virtmanager http://127.0.0.1:8765/mcp \
  --header "Authorization: Bearer change-me"
```

HTTP mode refuses to start without a token. Requests without a matching `Authorization: Bearer` header get `401`.

## Instructions for AI agents

Two templates tell an agent to install and test software in a development VM through this server
instead of on the host, keep an inventory of what the VM contains, and use snapshots and
disposable VMs when the management tools are enabled:

- `global-CLAUDE.md` for Claude Code: copy it into `~/.claude/CLAUDE.md`.
- `global-AGENTS.md` for agents that read `AGENTS.md` (e.g. Codex: `~/.codex/AGENTS.md`).

Adjust the VM name in them to yours. Both use the same inventory directory, so agents share it.

## Notes and limitations

- `exec` is not interactive: output is returned when the process exits. On timeout the server tries
  to kill the process (`kill -KILL` / `taskkill /F /T`). On Linux, children of the shell may survive.
- `console_read` returns output received since `console_open`. Earlier output (e.g. boot logs) is
  not available. Only one client can hold a console: if virt-manager's serial console view
  is open, `console_open` fails unless `force: true`, which disconnects the other client.
- `copy_to_vm` streams the file in 64 KiB guest-agent requests, so the data never passes through
  the conversation. The host path is resolved (symlinks, `..`) before the `host_share_dirs`
  check, so links pointing outside a share are refused. The guest file gets the guest agent's
  default permissions; run `chmod +x` through `exec` for scripts. The parent directory must exist.
- `stop_vm` without `force` sends a shutdown request and waits (60 s by default). A guest that
  ignores ACPI and has no guest agent will not shut down; use `force: true` to power it off.
- `type_text` assumes a US keyboard layout in the guest and supports printable ASCII only.
- Created VMs reference their disks by file path, like virt-manager. Volume-type disks
  (`<source pool=… volume=…/>`) failed to open under Ubuntu's AppArmor setup.
- Logs go to stderr; set verbosity with `RUST_LOG` (e.g. `RUST_LOG=debug`).
