//! Serial / virtio console sessions (the equivalent of `virsh console`).
//!
//! Each open session owns a non-blocking libvirt stream. A background thread
//! drains it into a bounded scrollback buffer addressed by absolute byte
//! offsets, so callers can read incrementally with a cursor.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use virt::domain::Domain;
use virt::stream::Stream;
use virt::sys;

use crate::hypervisor::Hypervisor;

const POLL: Duration = Duration::from_millis(30);

#[derive(Default)]
struct Buffer {
    data: VecDeque<u8>,
    /// Absolute offset of `data[0]`.
    start: u64,
    /// Set once the stream ended; holds the reason.
    closed: Option<String>,
}

struct Session {
    stream: Arc<Stream>,
    buffer: Arc<Mutex<Buffer>>,
    stop: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    device: Option<String>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
        unsafe { sys::virStreamAbort(self.stream.as_ptr()) };
    }
}

pub struct ReadResult {
    pub data: Vec<u8>,
    /// Offset to pass as `since` on the next read.
    pub next: u64,
    /// Bytes that were requested but already dropped from the scrollback.
    pub dropped: u64,
    pub closed: Option<String>,
}

#[derive(Default)]
pub struct Consoles {
    sessions: Mutex<HashMap<String, Session>>,
}

impl Consoles {
    pub fn open(&self, hv: &Hypervisor, vm: &str, dom: &Domain, device: Option<&str>, force: bool) -> Result<String> {
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(s) = sessions.get(vm) {
            if s.buffer.lock().unwrap().closed.is_none() {
                return Ok(format!(
                    "console for '{vm}' is already open (device: {})",
                    s.device.as_deref().unwrap_or("default")
                ));
            }
            sessions.remove(vm);
        }

        let stream = hv.with_conn(|conn| Ok(Stream::new(conn, sys::VIR_STREAM_NONBLOCK)?))?;
        let flags = if force { sys::VIR_DOMAIN_CONSOLE_FORCE } else { sys::VIR_DOMAIN_CONSOLE_SAFE };
        dom.open_console(device, &stream, flags).map_err(|e| {
            anyhow!(
                "cannot open console: {} (it may be in use, e.g. by virt-manager; retry with force=true to take it over)",
                e.message()
            )
        })?;

        let stream = Arc::new(stream);
        let buffer = Arc::new(Mutex::new(Buffer::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let cap = hv.config.limits.console_buffer_bytes;
        let reader = {
            let (stream, buffer, stop) = (stream.clone(), buffer.clone(), stop.clone());
            std::thread::Builder::new()
                .name(format!("console-{vm}"))
                .spawn(move || read_loop(&stream, &buffer, &stop, cap))?
        };
        sessions.insert(
            vm.to_string(),
            Session {
                stream,
                buffer,
                stop,
                reader: Some(reader),
                device: device.map(str::to_string),
            },
        );
        Ok(format!("console for '{vm}' opened; output is buffered from now on"))
    }

    /// Returns buffered output from absolute offset `since` (None = only the
    /// latest `tail` bytes). Waits up to `wait` for new data if none is available.
    pub fn read(&self, vm: &str, since: Option<u64>, max_bytes: usize, wait: Duration) -> Result<ReadResult> {
        let buffer = self.buffer(vm)?;
        let deadline = Instant::now() + wait;
        loop {
            {
                let b = buffer.lock().unwrap();
                let end = b.start + b.data.len() as u64;
                let from = match since {
                    Some(s) => s.min(end),
                    None => end.saturating_sub(max_bytes as u64).max(b.start),
                };
                let has_data = from < end;
                if has_data || b.closed.is_some() || Instant::now() >= deadline {
                    let dropped = b.start.saturating_sub(from);
                    let from = from.max(b.start);
                    let skip = (from - b.start) as usize;
                    let data: Vec<u8> = b.data.iter().skip(skip).take(max_bytes).copied().collect();
                    return Ok(ReadResult {
                        next: from + data.len() as u64,
                        data,
                        dropped,
                        closed: b.closed.clone(),
                    });
                }
            }
            std::thread::sleep(POLL);
        }
    }

    pub fn write(&self, vm: &str, data: &[u8]) -> Result<()> {
        let stream = {
            let sessions = self.sessions.lock().unwrap();
            let s = sessions
                .get(vm)
                .ok_or_else(|| anyhow!("no console open for '{vm}'; call console_open first"))?;
            if let Some(reason) = &s.buffer.lock().unwrap().closed {
                bail!("console for '{vm}' is closed: {reason}");
            }
            s.stream.clone()
        };
        let mut sent = 0;
        let deadline = Instant::now() + Duration::from_secs(10);
        while sent < data.len() {
            let rest = &data[sent..];
            let n = unsafe { sys::virStreamSend(stream.as_ptr(), rest.as_ptr() as *const _, rest.len()) };
            match n {
                n if n >= 0 => sent += n as usize,
                -2 if Instant::now() < deadline => std::thread::sleep(POLL),
                -2 => bail!("timed out writing to console"),
                _ => bail!("console write failed: {}", virt::error::Error::last_error().message()),
            }
        }
        Ok(())
    }

    pub fn is_open(&self, vm: &str) -> bool {
        self.sessions
            .lock()
            .unwrap()
            .get(vm)
            .is_some_and(|s| s.buffer.lock().unwrap().closed.is_none())
    }

    pub fn close(&self, vm: &str) -> bool {
        self.sessions.lock().unwrap().remove(vm).is_some()
    }

    fn buffer(&self, vm: &str) -> Result<Arc<Mutex<Buffer>>> {
        self.sessions
            .lock()
            .unwrap()
            .get(vm)
            .map(|s| s.buffer.clone())
            .ok_or_else(|| anyhow!("no console open for '{vm}'; call console_open first"))
    }
}

fn read_loop(stream: &Stream, buffer: &Mutex<Buffer>, stop: &AtomicBool, cap: usize) {
    let mut chunk = vec![0u8; 16 * 1024];
    let reason = loop {
        if stop.load(Ordering::Relaxed) {
            break "closed by client".to_string();
        }
        let n = unsafe { sys::virStreamRecv(stream.as_ptr(), chunk.as_mut_ptr() as *mut _, chunk.len()) };
        match n {
            0 => break "console stream ended (VM shut down or console taken over)".to_string(),
            -2 => std::thread::sleep(POLL),
            n if n > 0 => {
                let mut b = buffer.lock().unwrap();
                b.data.extend(&chunk[..n as usize]);
                let excess = b.data.len().saturating_sub(cap);
                if excess > 0 {
                    b.data.drain(..excess);
                    b.start += excess as u64;
                }
            }
            _ => break format!("console error: {}", virt::error::Error::last_error().message()),
        }
    };
    buffer.lock().unwrap().closed = Some(reason);
}
