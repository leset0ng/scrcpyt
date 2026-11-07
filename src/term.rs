//! Terminal setup, teardown and capability probing.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use crossterm::terminal;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    SharedMemory,
    Direct,
}

#[derive(Debug)]
pub struct Window {
    /// Usable area in pixels (a whole number of cells)
    pub width: u32,
    pub height: u32,
    pub cell_width: u32,
    pub cell_height: u32,
}

pub fn window() -> io::Result<Window> {
    let ws = terminal::window_size()?;
    let (cols, rows) = (ws.columns.max(1) as u32, ws.rows.max(1) as u32);
    // Assume a common cell size if the terminal does not report pixels
    let (cw, ch) = match (ws.width as u32 / cols, ws.height as u32 / rows) {
        (0, _) | (_, 0) => (8, 16),
        size => size,
    };
    Ok(Window { width: cols * cw, height: rows * ch, cell_width: cw, cell_height: ch })
}

const SETUP: &str = concat!(
    "\x1b[?1049h",                                  // alternate screen
    "\x1b[?25l",                                    // hide cursor
    "\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?1016h", // mouse: buttons + drag, SGR pixels
    "\x1b[?2004h",                                  // bracketed paste
    "\x1b[>1u",                                     // kitty keyboard: disambiguate escape codes
    "\x1b[22;0t",                                   // save title
);

const TEARDOWN: &str = concat!(
    "\x1b_Ga=d,d=A,q=2\x1b\\",
    "\x1b[<u",
    "\x1b[?2004l",
    "\x1b[?1016l\x1b[?1006l\x1b[?1002l\x1b[?1000l",
    "\x1b[?25h",
    "\x1b[?1049l",
    "\x1b[23;0t",
);

/// Raw mode + alternate screen, restored on drop.
pub struct Terminal;

impl Terminal {
    pub fn enter() -> Result<(Terminal, Transport)> {
        terminal::enable_raw_mode()?;
        let term = Terminal;
        let transport = probe()?;
        let mut out = io::stdout();
        out.write_all(SETUP.as_bytes())?;
        out.flush()?;
        Ok((term, transport))
    }

    pub fn set_title(title: &str) {
        let mut out = io::stdout();
        let _ = write!(out, "\x1b]2;{title}\x07");
        let _ = out.flush();
    }

    /// Copies text to the system clipboard through the terminal (OSC 52).
    pub fn set_clipboard(text: &str) {
        let mut out = io::stdout().lock();
        let _ = write!(out, "\x1b]52;c;{}\x07", B64.encode(text));
        let _ = out.flush();
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let mut out = io::stdout();
        let _ = out.write_all(TEARDOWN.as_bytes());
        let _ = out.flush();
        let _ = terminal::disable_raw_mode();
    }
}

/// Queries kitty graphics support, preferring shared memory. A trailing
/// primary device attributes query tells when the terminal has answered
/// everything it is going to answer.
fn probe() -> Result<Transport> {
    let shm_name = format!("/scrcpyt.{}.probe", std::process::id());
    let shm_ok = crate::render::write_shm(&shm_name, &[0, 0, 0]).is_ok();

    let mut q = String::from("\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\");
    if shm_ok {
        q += &format!("\x1b_Gi=32,s=1,v=1,a=q,t=s,f=24,S=3;{}\x1b\\", B64.encode(&shm_name));
    }
    q += "\x1b[c";
    let mut out = io::stdout();
    out.write_all(q.as_bytes())?;
    out.flush()?;

    let reply = read_until_da(Duration::from_secs(2));
    if shm_ok {
        let c = std::ffi::CString::new(shm_name).unwrap();
        unsafe { libc::shm_unlink(c.as_ptr()) };
    }

    let reply = String::from_utf8_lossy(&reply);
    if reply.contains("\x1b_Gi=32;OK") {
        Ok(Transport::SharedMemory)
    } else if reply.contains("\x1b_Gi=31;OK") {
        Ok(Transport::Direct)
    } else {
        bail!("this terminal does not support the kitty graphics protocol")
    }
}

fn read_until_da(timeout: Duration) -> Vec<u8> {
    let deadline = Instant::now() + timeout;
    let mut buf = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || !poll_stdin(left) {
            return buf;
        }
        let mut chunk = [0u8; 1024];
        let n = match read_stdin(&mut chunk) {
            Ok(0) | Err(_) => return buf,
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);
        if is_da_reply_end(&buf) {
            return buf;
        }
    }
}

fn is_da_reply_end(buf: &[u8]) -> bool {
    // ESC [ ? ... c
    buf.windows(3)
        .rposition(|w| w == b"\x1b[?")
        .is_some_and(|start| buf[start + 3..].iter().position(|&b| b == b'c').is_some())
}

/// Unbuffered read, so that `poll_stdin` never misses bytes left in a buffer.
pub fn read_stdin(buf: &mut [u8]) -> io::Result<usize> {
    let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}

pub fn poll_stdin(timeout: Duration) -> bool {
    let mut fd = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    unsafe { libc::poll(&mut fd, 1, ms) > 0 }
}
