//! Directory listing and stat. qemu-ga has no such commands, so these run fixed
//! helper programs through guest-exec. The path is always passed as data (an argv
//! entry on Linux, a quoted literal on Windows), never spliced into shell code.

use std::time::Duration;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::agent::{Agent, ExecSpec, GuestOs, powershell, ps_quote};

const HELPER_TIMEOUT: Duration = Duration::from_secs(30);
const HELPER_MAX_OUTPUT: usize = 8 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    /// "file", "directory", "symlink" or "other".
    #[serde(rename = "type")]
    pub kind: String,
    pub size: Option<u64>,
    pub modified_unix: Option<i64>,
    /// Octal permissions on Linux, attribute list on Windows.
    pub mode: Option<String>,
    pub owner: Option<String>,
}

const LINUX_FMT: &str = "%F|%s|%Y|%a|%U|%n";

const LINUX_LIST: &str = r#"cd -- "$1" || exit 2
find . -mindepth 1 -maxdepth 1 -exec stat -c "$2" -- {} +"#;

const PS_ENTRY: &str = r#"
function Entry($i) {
  [pscustomobject]@{
    name = $i.Name
    type = if ($i.LinkType) { 'symlink' } elseif ($i.PSIsContainer) { 'directory' } else { 'file' }
    size = if ($i.PSIsContainer) { $null } else { [uint64]$i.Length }
    modified_unix = ([DateTimeOffset]$i.LastWriteTimeUtc).ToUnixTimeSeconds()
    mode = $i.Attributes.ToString()
    owner = $null
  }
}
"#;

pub fn list_dir(agent: &Agent, path: &str) -> Result<Vec<Entry>> {
    match agent.os()? {
        GuestOs::Linux => {
            let out = run(agent, ExecSpec::new("/bin/sh", ["-c", LINUX_LIST, "sh", path, LINUX_FMT]))?;
            out.lines().filter(|l| !l.is_empty()).map(parse_linux).collect()
        }
        GuestOs::Windows => {
            let script = format!(
                "{PS_PRELUDE}{PS_ENTRY}\nConvertTo-Json -Compress -Depth 2 -InputObject @(Get-ChildItem -LiteralPath {} -Force | ForEach-Object {{ Entry $_ }})",
                ps_quote(path)
            );
            Ok(serde_json::from_str(run(agent, powershell(&script))?.trim())?)
        }
    }
}

pub fn stat(agent: &Agent, path: &str) -> Result<Entry> {
    match agent.os()? {
        GuestOs::Linux => {
            let out = run(agent, ExecSpec::new("stat", ["-c", LINUX_FMT, "--", path]))?;
            parse_linux(out.trim_end_matches('\n'))
        }
        GuestOs::Windows => {
            let script = format!(
                "{PS_PRELUDE}{PS_ENTRY}\nConvertTo-Json -Compress -Depth 2 -InputObject (Entry (Get-Item -LiteralPath {} -Force))",
                ps_quote(path)
            );
            Ok(serde_json::from_str(run(agent, powershell(&script))?.trim())?)
        }
    }
}

const PS_PRELUDE: &str =
    "$ErrorActionPreference = 'Stop'\n[Console]::OutputEncoding = [Text.Encoding]::UTF8\n";

fn run(agent: &Agent, spec: ExecSpec) -> Result<String> {
    let out = agent.exec(&spec, HELPER_TIMEOUT, HELPER_MAX_OUTPUT)?;
    if out.timed_out {
        bail!("helper command timed out in the guest");
    }
    if out.exit_code != Some(0) {
        bail!(
            "failed (exit code {:?}): {}",
            out.exit_code,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn parse_linux(line: &str) -> Result<Entry> {
    let f: Vec<&str> = line.splitn(6, '|').collect();
    if f.len() != 6 {
        bail!("unexpected stat output: {line}");
    }
    let kind = match f[0] {
        "regular file" | "regular empty file" => "file",
        "directory" => "directory",
        "symbolic link" => "symlink",
        _ => "other",
    };
    Ok(Entry {
        name: f[5].strip_prefix("./").unwrap_or(f[5]).to_string(),
        kind: kind.into(),
        size: f[1].parse().ok(),
        modified_unix: f[2].parse().ok(),
        mode: Some(f[3].to_string()),
        owner: Some(f[4].to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gnu_stat_line_with_pipe_in_name() {
        let e = parse_linux("regular file|42|1700000000|644|root|./a|b.txt").unwrap();
        assert_eq!((e.name.as_str(), e.kind.as_str(), e.size), ("a|b.txt", "file", Some(42)));
        assert_eq!(parse_linux("directory|4096|1|755|u|./d").unwrap().kind, "directory");
        assert!(parse_linux("garbage").is_err());
    }
}
