# virtmanager-mcp

An MCP server that gives an AI access to **already running** libvirt / virt-manager VMs:
command execution, file access, the serial console, screenshots and keyboard input.
It deliberately cannot start, stop, create or reconfigure VMs.

Host: Linux. Guests: Linux and Windows.

## How it talks to the guest

| Capability | Mechanism | Guest requirement |
|---|---|---|
| `exec`, `read_file`, `write_file`, `list_dir`, `stat_path`, `guest_info` | QEMU guest agent over a virtio-serial channel (`virDomainQemuAgentCommand`) | qemu-guest-agent running |
| `console_open/read/write/close` | Serial / virtio console stream (`virDomainOpenConsole`, like `virsh console`) | `<console>`/`<serial>` device + getty on it |
| `screenshot`, `send_keys`, `type_text` | Graphical display (`virDomainScreenshot`, `virDomainSendKey`) | A graphics device (SPICE/VNC) |

`list_dir` and `stat_path` run fixed helper commands through `guest-exec`
(`stat`/`find` on Linux, PowerShell on Windows), because qemu-ga has no directory listing.
The path is passed as data, never spliced into shell code.

## Tools

| Tool | Read-only mode |
|---|---|
| `list_vms`: running, allowed VMs, agent status, OS family | ✅ |
| `read_file`, `list_dir`, `stat_path` | ✅ |
| `console_open`, `console_read`, `console_close` | ✅ |
| `exec`: shell command (`sh`/`bash`/`cmd`/`powershell`) or program + args, with timeout | ❌ |
| `write_file` (overwrite/append, text or base64) | ❌ |
| `console_write` | ❌ |
| `guest_info`: OS, hostname, NICs, filesystems, users, timezone | ❌ |
| `screenshot` (PNG, downscaled to 1280 px wide by default) | ❌ |
| `send_keys` (`ctrl+alt+delete`, `win+r`, …), `type_text` (US layout) | ❌ |

In read-only mode the ❌ tools are not advertised at all.

## Build

Requires the libvirt development files (`libvirt` on Arch, `libvirt-dev` on Debian/Ubuntu,
`libvirt-devel` on Fedora).

```sh
cargo build --release
# binary: target/release/virtmanager-mcp
```

## Host setup

1. Access to `qemu:///system`: add your user to the `libvirt` group and log in again.
   ```sh
   sudo usermod -aG libvirt $USER
   ```
2. Configuration: copy `config.example.toml` to `~/.config/virtmanager-mcp/config.toml` and list
   your VMs in `allowed_vms`. **No VM is accessible until it is listed** (or `"*"` is used).
   CLI flags override the file: `--uri`, `--allow <VM>` (repeatable), `--read-only`,
   `--bind`, `--config`.

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

## Notes and limitations

- `exec` is not interactive: output is returned when the process exits. On timeout the server tries
  to kill the process (`kill -KILL` / `taskkill /F /T`). On Linux, children of the shell may survive.
- `console_read` returns output received since `console_open`. Earlier output (e.g. boot logs) is
  not available. Only one client can hold a console: if virt-manager's serial console view
  is open, `console_open` fails unless `force: true`, which disconnects the other client.
- `type_text` assumes a US keyboard layout in the guest and supports printable ASCII only.
- Logs go to stderr; set verbosity with `RUST_LOG` (e.g. `RUST_LOG=debug`).
