# Installing software: use the development VM

The host machine stays clean. The development VM is reached through the `virtmanager` MCP server
(tools `mcp__virtmanager__*`). Currently it is `ubuntu26.04`; confirm with `list_vms`.

## Using the VM

- **Programs needed for testing or development go into the VM, not onto the host.** This covers
  system packages, language runtimes, compilers, databases, services, CLI tools and anything that
  needs `sudo`. Install them with `exec` (it runs as root in the guest, so no `sudo` is needed).
- If the VM is shut off, start it with `start_vm` (`wait_for_agent_secs: 120`).
- **Shut the VM down when you are done** if you started it in this session: call `stop_vm` (without
  `force`) at the end of the task, unless the user asked to keep it running. Never stop a VM you
  did not start.
- **Keep the VM up to date.** The first time you use the VM on a given day, upgrade its packages
  (on Ubuntu: `DEBIAN_FRONTEND=noninteractive apt-get update && apt-get -y full-upgrade`, with a
  long `timeout_secs`), then write today's date as `Last system update` in the inventory. Skip
  this if the inventory already shows today's date. If the upgrade needs a reboot, tell the user
  instead of rebooting in the middle of a task, unless you started the VM yourself.
- **Run builds and tests as a normal user, not root.** `exec` runs as root, so wrap the command in
  `runuser -l <user> -c '...'`. Use root only for installing software and changing system
  configuration. Use the user recorded in the inventory; if none is recorded, use the existing
  regular user (UID 1000 or higher) and record it.
  Per-user toolchains (rustup, nvm, pipx, etc.) are installed as that user, not as root.
- **Test suites that change the system go in the VM too.** This applies when the tests need root,
  install or start services, change `/etc`, load kernel modules or networking, or come from an
  untrusted source. Run them in the VM even if the code is developed on the host.
- To work on a project inside the VM, copy files in with `copy_to_vm` (host files must be inside a
  configured share directory) or `write_file`, run the work with `exec`, and read results back with
  `read_file`.
- **If running something in the VM does not make sense, ask before installing it on the host.**
  Examples: a tool that must touch host files directly, editor or IDE integrations, or something
  that needs the host's hardware or display. Say what you want to install, why the VM will not
  work, and how it would be installed (package manager, `cargo install`, etc.). Do not install on
  the host without a yes.
- Project-local dependencies that the project's own build fetches into the project directory
  (`cargo build`, `npm install`, a `.venv`) are not "installing programs" and can be used on the
  host as normal.

## Managing VMs: snapshots and disposable VMs

The server may also offer VM management tools (`create_vm`, `delete_vm`, `snapshot_*`,
`configure_vm`, ...). They are off unless the user enabled them in the server config. If they are
missing, work without them and do not ask the user to enable them unless the task cannot be done
otherwise.

- **Snapshot before risky changes.** If `snapshot_create` is available and the VM shows
  `managed: true` in `list_vms`, take a snapshot before changes that are hard to undo (major
  upgrades, kernel or bootloader changes, system-wide configuration experiments). Revert with
  `snapshot_revert` if it goes wrong, and delete the snapshot with `snapshot_delete` once the
  change has worked out. Revert only to snapshots you took in this session, unless the user asks.
  A running UEFI VM may not be snapshottable; then shut it down first or skip the snapshot and
  say so.
- **After a revert, the inventory may be wrong.** Software installed after the snapshot is gone.
  Fix the entries you know changed; for the rest, follow the stale-record rule below.
- **Use a disposable VM** when the work could damage the development VM or should not run there:
  untrusted code, tests that break networking or the system, or a different OS or version than
  the development VM. Create it with `create_vm` (if available), do the work, then delete it with
  `delete_vm` at the end of the task unless the user asks to keep it. Delete only VMs you created
  in this session; ask before deleting any other VM.
- A new VM starts empty: it needs an OS installed from an ISO (`list_isos`) and the QEMU guest
  agent before `exec` and the file tools work. If that is more work than the task justifies, ask
  the user instead.
- Stay within the limits named in the server instructions; do not ask for them to be raised
  unless the task needs it.

## VM inventory: remember what is installed

Keep a record of what is installed in each VM so later sessions do not re-check or reinstall it.

- **Location:** `~/.claude/vm-inventory/<vm-name>.md`, shared by all projects. Create it if missing.
- **Read it first** whenever a task may need a tool in the VM. If the tool is listed, use it
  directly; do not run `which`, `--version` or the package manager just to confirm.
- **Update it right after** installing, upgrading or removing something, and when you find out
  that something is already there. One line per tool:

  ```markdown
  # ubuntu26.04
  OS: Ubuntu 26.04, package manager: apt
  Normal user: skorooblacik
  Last system update: 2026-10-15

  - rustc 1.94 (rustup for alice, ~/.cargo/bin) - 2026-10-15
  - postgresql 18 (apt), service enabled - 2026-10-15
  ```

  Include how it was installed and anything non-obvious (install path, PATH changes, services
  enabled, the user it was installed for).
- **The record can go stale**, for example when the VM is reverted to a snapshot. If a listed tool
  turns out to be missing or a different version, fix the entry, then install it. Do not re-check
  the whole list because of one wrong entry.
- **VMs you create get their own inventory file** as soon as they exist (OS, user, what you
  install). Delete the file when you delete the VM.
- Record only installed software and VM facts there (OS, package manager, users, paths). Project
  notes belong in that project's memory.
