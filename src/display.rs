//! Graphical console: screenshots and keyboard input.

use std::io::Cursor;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use image::{ImageFormat, imageops::FilterType};
use virt::domain::Domain;
use virt::stream::Stream;
use virt::sys;

use crate::hypervisor::Hypervisor;

/// Takes a screenshot and returns it as PNG bytes plus the final (width, height).
pub fn screenshot(hv: &Hypervisor, dom: &Domain, screen: u32, max_width: Option<u32>) -> Result<(Vec<u8>, u32, u32)> {
    let stream = hv.with_conn(|conn| Ok(Stream::new(conn, 0)?))?;
    let mime = dom
        .screenshot(&stream, screen, 0)
        .map_err(|e| anyhow!("screenshot failed: {} (does the VM have a graphical display?)", e.message()))?;
    let mut raw = Vec::new();
    let mut chunk = vec![0u8; 256 * 1024];
    loop {
        let n = stream.recv(&mut chunk).context("reading screenshot stream")?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..n]);
    }
    let _ = stream.finish();

    let format = match mime.as_str() {
        "image/png" => ImageFormat::Png,
        "image/x-portable-pixmap" => ImageFormat::Pnm,
        other => ImageFormat::from_mime_type(other).ok_or_else(|| anyhow!("unsupported screenshot format {other}"))?,
    };
    let mut img = image::load_from_memory_with_format(&raw, format).context("decoding screenshot")?;
    if let Some(w) = max_width
        && w > 0
        && img.width() > w
    {
        let h = (img.height() as u64 * w as u64 / img.width() as u64).max(1) as u32;
        img = img.resize_exact(w, h, FilterType::Triangle);
    }
    let mut png = Vec::new();
    img.write_to(&mut Cursor::new(&mut png), ImageFormat::Png)?;
    Ok((png, img.width(), img.height()))
}

/// Presses a set of keys simultaneously (e.g. ctrl+alt+delete) and releases them.
pub fn send_combo(dom: &Domain, keys: &[u32], hold_ms: u32) -> Result<()> {
    if keys.is_empty() {
        bail!("no keys given");
    }
    if keys.len() > sys::VIR_DOMAIN_SEND_KEY_MAX_KEYS as usize {
        bail!("at most {} keys per combo", sys::VIR_DOMAIN_SEND_KEY_MAX_KEYS);
    }
    let mut codes = keys.to_vec();
    dom.send_key(sys::VIR_KEYCODE_SET_LINUX, hold_ms, codes.as_mut_ptr(), codes.len() as i32, 0)
        .map_err(|e| anyhow!("send_key failed: {}", e.message()))
}

/// Parses "ctrl+alt+delete", "Enter", "shift+a", ... into Linux keycodes.
pub fn parse_combo(combo: &str) -> Result<Vec<u32>> {
    combo
        .split('+')
        .map(|k| {
            let k = k.trim();
            key_by_name(k).ok_or_else(|| anyhow!("unknown key '{k}' in '{combo}'"))
        })
        .collect()
}

/// Types text using a US keyboard layout.
pub fn type_text(dom: &Domain, text: &str, delay: Duration) -> Result<()> {
    // Validate everything up front so we never type half a string.
    let text = text.replace("\r\n", "\n");
    let strokes: Vec<Vec<u32>> = text
        .chars()
        .map(|c| {
            char_to_keys(c).ok_or_else(|| {
                anyhow!("character {c:?} cannot be typed with a US layout; use exec, write_file or console_write instead")
            })
        })
        .collect::<Result<_>>()?;
    for keys in strokes {
        send_combo(dom, &keys, 10)?;
        std::thread::sleep(delay);
    }
    Ok(())
}

// Linux input event codes (linux/input-event-codes.h).
const KEY_LEFTSHIFT: u32 = 42;

fn key_by_name(name: &str) -> Option<u32> {
    let n = name.to_ascii_lowercase();
    if n.chars().count() == 1 {
        let c = n.chars().next()?;
        if let Some((code, false)) = char_key(c) {
            return Some(code);
        }
    }
    if let Some(f) = n.strip_prefix('f').and_then(|d| d.parse::<u32>().ok()) {
        return match f {
            1..=10 => Some(58 + f),
            11 => Some(87),
            12 => Some(88),
            _ => None,
        };
    }
    Some(match n.as_str() {
        "esc" | "escape" => 1,
        "backspace" => 14,
        "tab" => 15,
        "enter" | "return" => 28,
        "ctrl" | "control" | "lctrl" => 29,
        "shift" | "lshift" => KEY_LEFTSHIFT,
        "rshift" => 54,
        "alt" | "lalt" => 56,
        "space" => 57,
        "capslock" => 58,
        "numlock" => 69,
        "scrolllock" => 70,
        "rctrl" => 97,
        "sysrq" | "printscreen" | "print" => 99,
        "ralt" | "altgr" => 100,
        "home" => 102,
        "up" => 103,
        "pageup" | "pgup" => 104,
        "left" => 105,
        "right" => 106,
        "end" => 107,
        "down" => 108,
        "pagedown" | "pgdn" => 109,
        "insert" | "ins" => 110,
        "delete" | "del" => 111,
        "pause" => 119,
        "meta" | "super" | "win" | "windows" | "lmeta" => 125,
        "rmeta" => 126,
        "menu" | "compose" => 127,
        "minus" => 12,
        "equal" => 13,
        _ => return None,
    })
}

fn char_to_keys(c: char) -> Option<Vec<u32>> {
    let (code, shift) = char_key(c)?;
    Some(if shift { vec![KEY_LEFTSHIFT, code] } else { vec![code] })
}

/// Maps a character to (keycode, needs_shift) for a US layout.
fn char_key(c: char) -> Option<(u32, bool)> {
    const ROW_QWERTY: [(char, u32); 10] =
        [('q', 16), ('w', 17), ('e', 18), ('r', 19), ('t', 20), ('y', 21), ('u', 22), ('i', 23), ('o', 24), ('p', 25)];
    const ROW_ASDF: [(char, u32); 9] =
        [('a', 30), ('s', 31), ('d', 32), ('f', 33), ('g', 34), ('h', 35), ('j', 36), ('k', 37), ('l', 38)];
    const ROW_ZXCV: [(char, u32); 7] = [('z', 44), ('x', 45), ('c', 46), ('v', 47), ('b', 48), ('n', 49), ('m', 50)];
    // (unshifted, shifted, code)
    const SYMBOLS: [(char, char, u32); 21] = [
        ('1', '!', 2), ('2', '@', 3), ('3', '#', 4), ('4', '$', 5), ('5', '%', 6),
        ('6', '^', 7), ('7', '&', 8), ('8', '*', 9), ('9', '(', 10), ('0', ')', 11),
        ('-', '_', 12), ('=', '+', 13), ('[', '{', 26), (']', '}', 27), (';', ':', 39),
        ('\'', '"', 40), ('`', '~', 41), ('\\', '|', 43), (',', '<', 51), ('.', '>', 52),
        ('/', '?', 53),
    ];
    match c {
        ' ' => return Some((57, false)),
        '\n' | '\r' => return Some((28, false)),
        '\t' => return Some((15, false)),
        _ => {}
    }
    let lower = c.to_ascii_lowercase();
    for (ch, code) in ROW_QWERTY.iter().chain(&ROW_ASDF).chain(&ROW_ZXCV) {
        if *ch == lower {
            return Some((*code, c.is_ascii_uppercase()));
        }
    }
    SYMBOLS.iter().find_map(|&(plain, shifted, code)| {
        if c == plain {
            Some((code, false))
        } else if c == shifted {
            Some((code, true))
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combos() {
        assert_eq!(parse_combo("ctrl+alt+delete").unwrap(), vec![29, 56, 111]);
        assert_eq!(parse_combo("Win+R").unwrap(), vec![125, 19]);
        assert_eq!(parse_combo("F12").unwrap(), vec![88]);
        assert_eq!(parse_combo("f1").unwrap(), vec![59]);
        assert!(parse_combo("ctrl+nope").is_err());
    }

    #[test]
    fn chars() {
        assert_eq!(char_to_keys('a'), Some(vec![30]));
        assert_eq!(char_to_keys('A'), Some(vec![KEY_LEFTSHIFT, 30]));
        assert_eq!(char_to_keys('|'), Some(vec![KEY_LEFTSHIFT, 43]));
        assert_eq!(char_to_keys('\n'), Some(vec![28]));
        assert_eq!(char_to_keys('é'), None);
    }
}
