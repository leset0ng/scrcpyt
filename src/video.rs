//! Demuxes the scrcpy video stream (see `app/src/demuxer.c`) and decodes it.

use std::io::{BufReader, Read};
use std::net::TcpStream;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ffmpeg_next::{self as ffmpeg, codec, decoder, frame};

const FLAG_SESSION: u8 = 0x80;
const FLAG_CONFIG: u64 = 1 << 62;
const FLAG_KEY_FRAME: u64 = 1 << 61;
const PTS_MASK: u64 = FLAG_KEY_FRAME - 1;

/// Latest decoded frame, shared with the renderer. Older frames are dropped.
#[derive(Default)]
pub struct FrameSlot {
    state: Mutex<SlotState>,
    cond: Condvar,
}

#[derive(Default)]
struct SlotState {
    frame: Option<frame::Video>,
    fresh: bool,
    closed: bool,
}

impl FrameSlot {
    fn push(&self, frame: frame::Video) {
        let mut s = self.state.lock().unwrap();
        s.frame = Some(frame);
        s.fresh = true;
        self.cond.notify_one();
    }

    pub fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.cond.notify_all();
    }

    /// Waits for a new frame. Returns `Err(())` once closed, `Ok(None)` on
    /// timeout, so the caller can still react to terminal resizes.
    pub fn wait(&self, timeout: Duration) -> Result<Option<frame::Video>, ()> {
        let s = self.state.lock().unwrap();
        let (mut s, _) = self.cond.wait_timeout_while(s, timeout, |s| !s.fresh && !s.closed).unwrap();
        if s.closed {
            return Err(());
        }
        if !s.fresh {
            return Ok(None);
        }
        s.fresh = false;
        Ok(s.frame.take())
    }
}

pub fn run(socket: TcpStream, slot: &FrameSlot) -> Result<()> {
    let mut r = BufReader::with_capacity(1 << 16, socket);

    let codec_id = read_u32(&mut r).context("could not read video codec")?;
    let id = match codec_id {
        0 => bail!("video stream disabled by the device"),
        1 => bail!("video stream configuration error on the device"),
        0x6832_3634 => codec::Id::H264,
        0x6832_3635 => codec::Id::HEVC,
        0x0061_7631 => codec::Id::AV1,
        other => bail!("unsupported video codec 0x{other:08x}"),
    };

    let mut header = [0u8; 12];
    r.read_exact(&mut header)?;
    if header[0] & FLAG_SESSION == 0 {
        bail!("unexpected video packet (expected a session header)");
    }

    let mut decoder = open_decoder(id)?;
    let mut config: Option<Vec<u8>> = None;
    let mut decoded = frame::Video::empty();

    loop {
        r.read_exact(&mut header).context("video stream ended")?;
        if header[0] & FLAG_SESSION != 0 {
            // New session (rotation, resize...): the following packets start
            // a new stream, the decoder picks up the new size by itself
            continue;
        }

        let pts_flags = u64::from_be_bytes(header[..8].try_into().unwrap());
        let len = u32::from_be_bytes(header[8..].try_into().unwrap()) as usize;
        if len == 0 {
            bail!("invalid video packet length: 0");
        }
        let mut data = vec![0u8; len];
        r.read_exact(&mut data)?;

        if pts_flags & FLAG_CONFIG != 0 {
            // H.26x config packets (SPS/PPS) are prepended to the next packet
            config = Some(data);
            continue;
        }
        if let Some(mut c) = config.take() {
            c.extend_from_slice(&data);
            data = c;
        }

        let mut packet = ffmpeg::Packet::copy(&data);
        packet.set_pts(Some((pts_flags & PTS_MASK) as i64));
        packet.set_dts(packet.pts());
        if pts_flags & FLAG_KEY_FRAME != 0 {
            packet.set_flags(codec::packet::Flags::KEY);
        }

        decoder.send_packet(&packet).context("could not send video packet")?;
        while decoder.receive_frame(&mut decoded).is_ok() {
            slot.push(std::mem::replace(&mut decoded, frame::Video::empty()));
        }
    }
}

fn open_decoder(id: codec::Id) -> Result<decoder::Video> {
    let codec = decoder::find(id).with_context(|| format!("no decoder for {id:?}"))?;
    let mut ctx = codec::Context::new_with_codec(codec);
    unsafe {
        (*ctx.as_mut_ptr()).flags |= ffmpeg::ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
    }
    Ok(ctx.decoder().video()?)
}

fn read_u32(r: &mut impl Read) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}
