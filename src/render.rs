//! Draws decoded frames with the kitty graphics protocol.

use std::ffi::CString;
use std::io::{self, Write};
use std::sync::Mutex;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use ffmpeg_next::format::Pixel;
use ffmpeg_next::frame;
use ffmpeg_next::software::scaling;

use crate::term::{self, Transport};
use crate::video::FrameSlot;

const IMAGE_IDS: [u32; 2] = [1, 2];
const CHUNK: usize = 4096;

/// Where the video is drawn, in terminal pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub video_width: u32,
    pub video_height: u32,
}

impl Placement {
    /// Maps a terminal pixel to video coordinates, clamped to the video.
    pub fn map(&self, px: i32, py: i32) -> (i32, i32, bool) {
        let rx = px - self.x as i32;
        let ry = py - self.y as i32;
        let inside = rx >= 0 && ry >= 0 && rx < self.width as i32 && ry < self.height as i32;
        let vx = rx.clamp(0, self.width as i32 - 1) as i64 * self.video_width as i64 / self.width as i64;
        let vy = ry.clamp(0, self.height as i32 - 1) as i64 * self.video_height as i64 / self.height as i64;
        (vx as i32, vy as i32, inside)
    }
}

pub fn run(slot: &FrameSlot, placement: &Mutex<Option<Placement>>, transport: Transport) -> io::Result<()> {
    let mut r = Renderer::new(transport);
    let mut last: Option<frame::Video> = None;
    loop {
        let fresh = match slot.wait(Duration::from_millis(100)) {
            Err(()) => return Ok(()),
            Ok(Some(f)) => {
                last = Some(f);
                true
            }
            Ok(None) => false,
        };
        let Some(frame) = &last else { continue };
        if let Some(p) = r.render(frame, fresh)? {
            *placement.lock().unwrap() = Some(p);
        }
    }
}

struct Renderer {
    transport: Transport,
    scaler: Option<(ScalerKey, scaling::Context)>,
    rgb: frame::Video,
    pixels: Vec<u8>,
    out: Vec<u8>,
    placement: Option<Placement>,
    /// Image currently on screen, 0 if none
    image_id: u32,
    shm_seq: u32,
}

#[derive(PartialEq, Eq)]
struct ScalerKey(Pixel, u32, u32, u32, u32);

impl Renderer {
    fn new(transport: Transport) -> Self {
        Renderer {
            transport,
            scaler: None,
            rgb: frame::Video::empty(),
            pixels: Vec::new(),
            out: Vec::new(),
            placement: None,
            image_id: 0,
            shm_seq: 0,
        }
    }

    /// Draws the frame if it is new or if the layout changed. Returns the new
    /// placement when it changed.
    fn render(&mut self, frame: &frame::Video, fresh: bool) -> io::Result<Option<Placement>> {
        let win = term::window()?;
        let p = fit(&win, frame.width(), frame.height());
        let moved = self.placement != Some(p);
        if !fresh && !moved {
            return Ok(None);
        }
        if moved {
            debug!("window {win:?}, placement {p:?}");
        }

        self.scale(frame, p.width, p.height)?;
        self.out.clear();
        // Synchronized output: the terminal shows the whole update at once
        self.out.extend_from_slice(b"\x1b[?2026h");
        if moved {
            // Drop the previous placement and any leftover from a resize
            self.out.extend_from_slice(b"\x1b_Ga=d,d=A,q=2\x1b\\\x1b[2J");
        }
        // Alternate between two images and delete the old one only once the
        // new one is placed, so that the screen never shows neither
        let shown = self.image_id;
        self.image_id = if shown == IMAGE_IDS[0] { IMAGE_IDS[1] } else { IMAGE_IDS[0] };
        let (col, xoff) = (p.x / win.cell_width, p.x % win.cell_width);
        let (row, yoff) = (p.y / win.cell_height, p.y % win.cell_height);
        write!(self.out, "\x1b[{};{}H", row + 1, col + 1)?;
        let keys = format!("a=T,f=24,s={},v={},i={},p=1,X={xoff},Y={yoff},C=1,q=2", p.width, p.height, self.image_id);
        match self.transport {
            Transport::SharedMemory => self.encode_shm(&keys)?,
            Transport::Direct => self.encode_direct(&keys),
        }
        if shown != 0 {
            write!(self.out, "\x1b_Ga=d,d=I,i={shown},q=2\x1b\\")?;
        }
        self.out.extend_from_slice(b"\x1b[?2026l");

        let mut stdout = io::stdout().lock();
        stdout.write_all(&self.out)?;
        stdout.flush()?;

        self.placement = Some(p);
        Ok(moved.then_some(p))
    }

    fn scale(&mut self, frame: &frame::Video, width: u32, height: u32) -> io::Result<()> {
        let key = ScalerKey(frame.format(), frame.width(), frame.height(), width, height);
        if self.scaler.as_ref().is_none_or(|(k, _)| *k != key) {
            let ctx = scaling::Context::get(
                frame.format(),
                frame.width(),
                frame.height(),
                Pixel::RGB24,
                width,
                height,
                scaling::Flags::BILINEAR,
            )
            .map_err(io::Error::other)?;
            self.scaler = Some((key, ctx));
            // The scaler rejects an output frame allocated for another size
            self.rgb = frame::Video::empty();
        }
        let (_, ctx) = self.scaler.as_mut().unwrap();
        ctx.run(frame, &mut self.rgb).map_err(io::Error::other)?;

        // swscale pads rows; the terminal expects tightly packed pixels
        let row = width as usize * 3;
        let stride = self.rgb.stride(0);
        let data = self.rgb.data(0);
        self.pixels.clear();
        for y in 0..height as usize {
            self.pixels.extend_from_slice(&data[y * stride..y * stride + row]);
        }
        Ok(())
    }

    fn encode_direct(&mut self, keys: &str) {
        let payload = B64.encode(&self.pixels);
        let chunks: Vec<&[u8]> = payload.as_bytes().chunks(CHUNK).collect();
        for (i, chunk) in chunks.iter().enumerate() {
            let more = (i + 1 < chunks.len()) as u8;
            if i == 0 {
                let _ = write!(self.out, "\x1b_G{keys},m={more};");
            } else {
                let _ = write!(self.out, "\x1b_Gm={more};");
            }
            self.out.extend_from_slice(chunk);
            self.out.extend_from_slice(b"\x1b\\");
        }
    }

    fn encode_shm(&mut self, keys: &str) -> io::Result<()> {
        self.shm_seq = self.shm_seq.wrapping_add(1);
        // macOS limits shm names to 31 bytes
        let name = format!("/scrcpyt.{}.{}", std::process::id(), self.shm_seq);
        write_shm(&name, &self.pixels)?;
        // The terminal unlinks the object once it has read it
        write!(self.out, "\x1b_G{keys},t=s,S={};{}\x1b\\", self.pixels.len(), B64.encode(&name))
    }
}

/// Fits the video in the window, centered, preserving its aspect ratio.
fn fit(win: &term::Window, vw: u32, vh: u32) -> Placement {
    let (aw, ah) = (win.width.max(1), win.height.max(1));
    let (w, h) = if aw as u64 * vh as u64 <= ah as u64 * vw as u64 {
        (aw, (aw as u64 * vh as u64 / vw as u64) as u32)
    } else {
        ((ah as u64 * vw as u64 / vh as u64) as u32, ah)
    };
    let (w, h) = (w.max(1), h.max(1));
    Placement { x: (aw - w) / 2, y: (ah - h) / 2, width: w, height: h, video_width: vw, video_height: vh }
}

pub fn write_shm(name: &str, data: &[u8]) -> io::Result<()> {
    let cname = CString::new(name).unwrap();
    unsafe {
        let fd = libc::shm_open(cname.as_ptr(), libc::O_CREAT | libc::O_EXCL | libc::O_RDWR, 0o600 as libc::c_uint);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let result = (|| {
            if libc::ftruncate(fd, data.len() as libc::off_t) != 0 {
                return Err(io::Error::last_os_error());
            }
            let ptr = libc::mmap(
                std::ptr::null_mut(),
                data.len(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            if ptr == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr as *mut u8, data.len());
            libc::munmap(ptr, data.len());
            Ok(())
        })();
        libc::close(fd);
        if result.is_err() {
            libc::shm_unlink(cname.as_ptr());
        }
        result
    }
}
