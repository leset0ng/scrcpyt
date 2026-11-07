//! Serialization of scrcpy control messages (see `app/src/control_msg.c`).

use std::io::{self, Read, Write};
use std::net::TcpStream;

pub mod keycode {
    pub const HOME: u32 = 3;
    pub const BACK: u32 = 4;
    pub const NUM_0: u32 = 7;
    pub const DPAD_UP: u32 = 19;
    pub const DPAD_DOWN: u32 = 20;
    pub const DPAD_LEFT: u32 = 21;
    pub const DPAD_RIGHT: u32 = 22;
    pub const VOLUME_UP: u32 = 24;
    pub const VOLUME_DOWN: u32 = 25;
    pub const POWER: u32 = 26;
    pub const A: u32 = 29;
    pub const TAB: u32 = 61;
    pub const SPACE: u32 = 62;
    pub const ENTER: u32 = 66;
    pub const DEL: u32 = 67;
    pub const MENU: u32 = 82;
    pub const PAGE_UP: u32 = 92;
    pub const PAGE_DOWN: u32 = 93;
    pub const ESCAPE: u32 = 111;
    pub const FORWARD_DEL: u32 = 112;
    pub const MOVE_HOME: u32 = 122;
    pub const MOVE_END: u32 = 123;
    pub const INSERT: u32 = 124;
    pub const F1: u32 = 131;
    pub const APP_SWITCH: u32 = 187;
}

pub mod meta {
    pub const SHIFT: u32 = 0x01 | 0x40;
    pub const ALT: u32 = 0x02 | 0x10;
    pub const CTRL: u32 = 0x1000 | 0x2000;
}

pub mod motion {
    pub const DOWN: u8 = 0;
    pub const UP: u8 = 1;
    pub const MOVE: u8 = 2;
    pub const BUTTON_PRIMARY: u32 = 1;
}

pub const POINTER_ID_MOUSE: u64 = u64::MAX;
const INJECT_TEXT_MAX_LEN: usize = 300;
const CLIPBOARD_TEXT_MAX_LEN: usize = (1 << 18) - 14;
/// Asks the device not to acknowledge a clipboard change
const SEQUENCE_INVALID: u64 = 0;

#[derive(Clone, Copy, Debug)]
pub struct Position {
    pub x: i32,
    pub y: i32,
    /// Video size the coordinates refer to; the device drops events whose
    /// size does not match the current stream (e.g. after a rotation).
    pub width: u16,
    pub height: u16,
}

pub enum Msg<'a> {
    Keycode {
        down: bool,
        keycode: u32,
        metastate: u32,
    },
    Text(&'a str),
    Touch {
        action: u8,
        position: Position,
        action_button: u32,
        buttons: u32,
    },
    Scroll {
        position: Position,
        hscroll: f32,
        vscroll: f32,
    },
    BackOrScreenOn {
        down: bool,
    },
    ExpandNotificationPanel,
    CollapsePanels,
    /// Sets the device clipboard, then optionally injects a paste
    SetClipboard {
        text: &'a str,
        paste: bool,
    },
    SetDisplayPower(bool),
    RotateDevice,
}

impl Msg<'_> {
    fn serialize(&self, buf: &mut Vec<u8>) {
        match *self {
            Msg::Keycode { down, keycode, metastate } => {
                buf.push(0);
                buf.push(!down as u8);
                buf.extend(keycode.to_be_bytes());
                buf.extend(0u32.to_be_bytes()); // repeat
                buf.extend(metastate.to_be_bytes());
            }
            Msg::Text(text) => {
                buf.push(1);
                let text = truncate_utf8(text, INJECT_TEXT_MAX_LEN);
                buf.extend((text.len() as u32).to_be_bytes());
                buf.extend(text.as_bytes());
            }
            Msg::Touch { action, position, action_button, buttons } => {
                buf.push(2);
                buf.push(action);
                buf.extend(POINTER_ID_MOUSE.to_be_bytes());
                write_position(buf, position);
                let pressure: u16 = if action == motion::UP { 0 } else { 0xffff };
                buf.extend(pressure.to_be_bytes());
                buf.extend(action_button.to_be_bytes());
                buf.extend(buttons.to_be_bytes());
            }
            Msg::Scroll { position, hscroll, vscroll } => {
                buf.push(3);
                write_position(buf, position);
                buf.extend(i16_fixed(hscroll / 16.0).to_be_bytes());
                buf.extend(i16_fixed(vscroll / 16.0).to_be_bytes());
                buf.extend(0u32.to_be_bytes()); // buttons
            }
            Msg::BackOrScreenOn { down } => buf.extend([4, !down as u8]),
            Msg::ExpandNotificationPanel => buf.push(5),
            Msg::CollapsePanels => buf.push(7),
            Msg::SetClipboard { text, paste } => {
                buf.push(9);
                buf.extend(SEQUENCE_INVALID.to_be_bytes());
                buf.push(paste as u8);
                let text = truncate_utf8(text, CLIPBOARD_TEXT_MAX_LEN);
                buf.extend((text.len() as u32).to_be_bytes());
                buf.extend(text.as_bytes());
            }
            Msg::SetDisplayPower(on) => buf.extend([10, on as u8]),
            Msg::RotateDevice => buf.push(11),
        }
    }
}

pub struct Controller {
    socket: TcpStream,
    buf: Vec<u8>,
}

impl Controller {
    pub fn new(socket: TcpStream) -> Self {
        Controller { socket, buf: Vec::new() }
    }

    pub fn send(&mut self, msg: Msg) -> std::io::Result<()> {
        self.buf.clear();
        msg.serialize(&mut self.buf);
        self.socket.write_all(&self.buf)
    }

    pub fn key(&mut self, keycode: u32, metastate: u32) -> std::io::Result<()> {
        self.send(Msg::Keycode { down: true, keycode, metastate })?;
        self.send(Msg::Keycode { down: false, keycode, metastate })
    }

    pub fn text(&mut self, text: &str) -> std::io::Result<()> {
        let mut rest = text;
        while !rest.is_empty() {
            let chunk = truncate_utf8(rest, INJECT_TEXT_MAX_LEN);
            self.send(Msg::Text(chunk))?;
            rest = &rest[chunk.len()..];
        }
        Ok(())
    }
}

/// Messages from the device (see `app/src/device_msg.c`).
pub enum DeviceMsg {
    Clipboard(String),
    Other,
}

impl DeviceMsg {
    pub fn read(r: &mut impl Read) -> io::Result<DeviceMsg> {
        let mut ty = [0u8; 1];
        r.read_exact(&mut ty)?;
        match ty[0] {
            0 => {
                let len = read_u32(r)? as usize;
                let mut text = vec![0u8; len];
                r.read_exact(&mut text)?;
                Ok(DeviceMsg::Clipboard(String::from_utf8_lossy(&text).into_owned()))
            }
            1 => {
                r.read_exact(&mut [0u8; 8])?; // clipboard ack sequence
                Ok(DeviceMsg::Other)
            }
            2 => {
                let mut head = [0u8; 4]; // uhid id, size
                r.read_exact(&mut head)?;
                let size = u16::from_be_bytes([head[2], head[3]]);
                io::copy(&mut r.take(size as u64), &mut io::sink())?;
                Ok(DeviceMsg::Other)
            }
            // The stream cannot be resynchronized
            other => Err(io::Error::other(format!("unknown device message type {other}"))),
        }
    }
}

fn read_u32(r: &mut impl Read) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

fn write_position(buf: &mut Vec<u8>, p: Position) {
    buf.extend(p.x.to_be_bytes());
    buf.extend(p.y.to_be_bytes());
    buf.extend(p.width.to_be_bytes());
    buf.extend(p.height.to_be_bytes());
}

fn i16_fixed(f: f32) -> i16 {
    (f.clamp(-1.0, 1.0) * 32768.0).min(32767.0) as i16
}

fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_clipboard() {
        let mut buf = Vec::new();
        Msg::SetClipboard { text: "你好", paste: true }.serialize(&mut buf);
        let mut want = vec![9, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 6];
        want.extend("你好".as_bytes());
        assert_eq!(buf, want);
    }

    #[test]
    fn device_msgs() {
        let mut stream: &[u8] = &[
            1, 0, 0, 0, 0, 0, 0, 0, 7, // ack
            2, 0, 1, 0, 2, 0xaa, 0xbb, // uhid output
            0, 0, 0, 0, 2, b'h', b'i', // clipboard
        ];
        assert!(matches!(DeviceMsg::read(&mut stream).unwrap(), DeviceMsg::Other));
        assert!(matches!(DeviceMsg::read(&mut stream).unwrap(), DeviceMsg::Other));
        assert!(matches!(DeviceMsg::read(&mut stream).unwrap(), DeviceMsg::Clipboard(t) if t == "hi"));
        assert!(DeviceMsg::read(&mut stream).is_err());
    }
}
