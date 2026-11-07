macro_rules! debug {
    ($($arg:tt)*) => { $crate::debug_log(format_args!($($arg)*)) };
}

mod control;
mod input;
mod render;
mod server;
mod term;
mod video;

use std::collections::HashSet;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::net::Shutdown;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;

use control::{Controller, DeviceMsg, Msg, Position, keycode, meta, motion};
use input::{Button, Event, Key, Mods, MouseKind};
use render::Placement;
use term::Terminal;
use video::FrameSlot;

/// Mirror and control an Android device in the terminal.
///
/// Mouse: left = touch, right = back, middle = home, wheel = scroll.
/// Keys: Alt+h home, Alt+b back, Alt+s app switch, Alt+m menu, Alt+p power,
/// Alt+n / Alt+Shift+n notification panel, Alt+o / Alt+Shift+o screen off/on,
/// Alt+r rotate, Alt+Up/Down volume, Ctrl+q quit.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Device serial, as listed by `adb devices`
    #[arg(short, long)]
    serial: Option<String>,

    /// Use the USB device (error if multiple devices connected)
    #[arg(short = 'd', long, conflicts_with_all = ["serial", "select_emulator"])]
    select_usb: bool,

    /// Use the emulator (error if multiple emulators running)
    #[arg(short = 'e', long, conflicts_with = "serial")]
    select_emulator: bool,

    /// Limit the video's largest dimension [default: terminal size in pixels]
    #[arg(short, long)]
    max_size: Option<u16>,

    /// Limit the frame rate
    #[arg(long)]
    max_fps: Option<String>,
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("scrcpyt: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> Result<()> {
    ffmpeg_next::init().context("could not initialize FFmpeg")?;
    // FFmpeg logs to stderr, which would scribble over the video
    ffmpeg_next::util::log::set_level(ffmpeg_next::util::log::Level::Error);

    let max_size = match args.max_size {
        Some(m) => m,
        None => {
            let w = term::window()?;
            w.width.max(w.height).min(u16::MAX as u32) as u16
        }
    };
    let opts = server::Options {
        serial: args.serial,
        select_usb: args.select_usb,
        select_emulator: args.select_emulator,
        max_size,
        max_fps: args.max_fps,
    };
    let (mut server, conn) = server::Server::start(&opts)?;

    let (term, transport) = Terminal::enter()?;
    Terminal::set_title(&format!("scrcpyt · {}", conn.device_name));

    let slot = FrameSlot::default();
    let placement = Mutex::new(None);
    let quit = AtomicBool::new(false);
    let video_sock = conn.video.try_clone()?;
    let control_sock = conn.control.try_clone()?;

    let result = thread::scope(|s| {
        let video = s.spawn(|| {
            let r = video::run(conn.video, &slot);
            slot.close();
            quit.store(true, Ordering::Relaxed);
            r
        });
        let render = s.spawn(|| {
            let r = render::run(&slot, &placement, transport);
            quit.store(true, Ordering::Relaxed);
            r
        });
        let mut device_msgs = io::BufReader::new(conn.control.try_clone()?);
        s.spawn(move || {
            // The device sends its clipboard whenever it changes
            while let Ok(msg) = DeviceMsg::read(&mut device_msgs) {
                if let DeviceMsg::Clipboard(text) = msg {
                    debug!("device clipboard: {} bytes", text.len());
                    Terminal::set_clipboard(&text);
                }
            }
        });

        let mut ctl = Controller::new(conn.control);
        let input_result = input_loop(&mut ctl, &mut server, &placement, &quit);

        let user_quit = !quit.swap(true, Ordering::Relaxed) && input_result.is_ok();
        let _ = video_sock.shutdown(Shutdown::Both);
        let _ = control_sock.shutdown(Shutdown::Both);
        slot.close();

        let video_result = video.join().unwrap();
        render.join().unwrap().context("could not render")?;
        input_result?;
        if user_quit { Ok(()) } else { video_result }
    });

    drop(term);
    let log = server.log();
    let mut seen = HashSet::new();
    for line in &log {
        debug!("server: {line}");
        // e.g. missing input injection permission, otherwise silent
        if line.contains("ERROR") && seen.insert(line) {
            eprintln!("{line}");
        }
    }
    result
}

/// Appends a line to the file named by `SCRCPYT_LOG`, if set.
fn debug_log(args: fmt::Arguments) {
    static LOG: OnceLock<Option<Mutex<File>>> = OnceLock::new();
    let log = LOG.get_or_init(|| {
        let path = std::env::var_os("SCRCPYT_LOG")?;
        OpenOptions::new().create(true).append(true).open(path).ok().map(Mutex::new)
    });
    if let Some(file) = log {
        let _ = writeln!(file.lock().unwrap(), "{args}");
    }
}

/// Returns `Ok` when the user quits or another thread stops, `Err` when the
/// device is lost.
fn input_loop(
    ctl: &mut Controller,
    server: &mut server::Server,
    placement: &Mutex<Option<Placement>>,
    quit: &AtomicBool,
) -> Result<()> {
    let mut parser = input::Parser::default();
    let mut events = Vec::new();
    let mut state = InputState::default();
    let mut buf = [0u8; 4096];

    while !quit.load(Ordering::Relaxed) {
        server.check()?;
        if term::poll_stdin(Duration::from_millis(30)) {
            let n = term::read_stdin(&mut buf)?;
            if n == 0 {
                return Ok(());
            }
            parser.feed(&buf[..n], &mut events);
        } else {
            parser.flush(&mut events);
        }

        let placement = *placement.lock().unwrap();
        for ev in events.drain(..) {
            match state.dispatch(ctl, placement, ev) {
                Ok(Flow::Continue) => {}
                Ok(Flow::Quit) => return Ok(()),
                Err(e) => return Err(e).context("lost the connection to the device"),
            }
        }
    }
    Ok(())
}

enum Flow {
    Continue,
    Quit,
}

#[derive(Default)]
struct InputState {
    /// Video size at touch down, so that a gesture stays consistent
    touching: Option<Placement>,
}

impl InputState {
    fn dispatch(&mut self, ctl: &mut Controller, placement: Option<Placement>, ev: Event) -> io::Result<Flow> {
        match ev {
            Event::Key(Key::Char('q'), Mods { ctrl: true, alt: false, .. }) => return Ok(Flow::Quit),
            Event::Key(k, mods) if mods.alt && !mods.ctrl => shortcut(ctl, k, mods)?,
            Event::Key(k, mods) => key(ctl, k, mods)?,
            // Through the device clipboard: injecting text drops non-ASCII
            // characters on most keyboards
            Event::Paste(text) => ctl.send(Msg::SetClipboard { text: &text, paste: true })?,
            Event::Mouse(kind, x, y) => {
                if let Some(p) = placement {
                    self.mouse(ctl, p, kind, x, y)?;
                }
            }
        }
        Ok(Flow::Continue)
    }

    fn mouse(&mut self, ctl: &mut Controller, p: Placement, kind: MouseKind, x: i32, y: i32) -> io::Result<()> {
        let at = |p: Placement| {
            let (vx, vy, inside) = p.map(x, y);
            let position = Position { x: vx, y: vy, width: p.video_width as u16, height: p.video_height as u16 };
            (position, inside)
        };
        let (position, inside) = at(p);
        debug!("mouse {kind:?} at {x},{y} -> {position:?} inside={inside}");
        let touch = |action, action_button, buttons, position| Msg::Touch { action, position, action_button, buttons };
        let primary = motion::BUTTON_PRIMARY;

        match kind {
            MouseKind::Press(Button::Left) if inside => {
                self.touching = Some(p);
                ctl.send(touch(motion::DOWN, primary, primary, position))?;
            }
            MouseKind::Drag(Button::Left) => {
                if let Some(tp) = self.touching {
                    ctl.send(touch(motion::MOVE, 0, primary, at(tp).0))?;
                }
            }
            MouseKind::Release(Button::Left) => {
                if let Some(tp) = self.touching.take() {
                    ctl.send(touch(motion::UP, primary, 0, at(tp).0))?;
                }
            }
            MouseKind::Press(Button::Right) => ctl.send(Msg::BackOrScreenOn { down: true })?,
            MouseKind::Release(Button::Right) => ctl.send(Msg::BackOrScreenOn { down: false })?,
            MouseKind::Press(Button::Middle) => {
                ctl.send(Msg::Keycode { down: true, keycode: keycode::HOME, metastate: 0 })?
            }
            MouseKind::Release(Button::Middle) => {
                ctl.send(Msg::Keycode { down: false, keycode: keycode::HOME, metastate: 0 })?
            }
            MouseKind::Scroll(h, v) if inside => {
                ctl.send(Msg::Scroll { position, hscroll: h as f32, vscroll: v as f32 })?
            }
            _ => {}
        }
        Ok(())
    }
}

fn shortcut(ctl: &mut Controller, k: Key, mods: Mods) -> io::Result<()> {
    let k = match k {
        Key::Char(c) => Key::Char(c.to_ascii_lowercase()),
        k => k,
    };
    let shift = mods.shift || matches!(k, Key::Char(c) if c.is_ascii_uppercase());
    match k {
        Key::Char('h') => ctl.key(keycode::HOME, 0),
        Key::Char('b') => ctl.key(keycode::BACK, 0),
        Key::Char('s') => ctl.key(keycode::APP_SWITCH, 0),
        Key::Char('m') => ctl.key(keycode::MENU, 0),
        Key::Char('p') => ctl.key(keycode::POWER, 0),
        Key::Char('n') if shift => ctl.send(Msg::CollapsePanels),
        Key::Char('n') => ctl.send(Msg::ExpandNotificationPanel),
        Key::Char('o') => ctl.send(Msg::SetDisplayPower(shift)),
        Key::Char('r') => ctl.send(Msg::RotateDevice),
        Key::Up => ctl.key(keycode::VOLUME_UP, 0),
        Key::Down => ctl.key(keycode::VOLUME_DOWN, 0),
        _ => Ok(()),
    }
}

fn key(ctl: &mut Controller, k: Key, mods: Mods) -> io::Result<()> {
    if let Key::Char(c) = k
        && !mods.ctrl
    {
        let mut b = [0u8; 4];
        return ctl.text(c.encode_utf8(&mut b));
    }

    let code = match k {
        Key::Char(c) => match c.to_ascii_lowercase() {
            c @ 'a'..='z' => keycode::A + (c as u32 - 'a' as u32),
            c @ '0'..='9' => keycode::NUM_0 + (c as u32 - '0' as u32),
            ' ' => keycode::SPACE,
            _ => return Ok(()),
        },
        Key::Enter => keycode::ENTER,
        Key::Tab => keycode::TAB,
        Key::Backspace => keycode::DEL,
        Key::Esc => keycode::ESCAPE,
        Key::Up => keycode::DPAD_UP,
        Key::Down => keycode::DPAD_DOWN,
        Key::Left => keycode::DPAD_LEFT,
        Key::Right => keycode::DPAD_RIGHT,
        Key::Home => keycode::MOVE_HOME,
        Key::End => keycode::MOVE_END,
        Key::PageUp => keycode::PAGE_UP,
        Key::PageDown => keycode::PAGE_DOWN,
        Key::Insert => keycode::INSERT,
        Key::Delete => keycode::FORWARD_DEL,
        Key::F(n) => keycode::F1 + (n as u32 - 1),
    };
    let mut metastate = 0;
    if mods.shift {
        metastate |= meta::SHIFT;
    }
    if mods.alt {
        metastate |= meta::ALT;
    }
    if mods.ctrl {
        metastate |= meta::CTRL;
    }
    ctl.key(code, metastate)
}
