//! Minimal terminal input parser: UTF-8 text, legacy and kitty keyboard
//! sequences, SGR mouse reports (in pixels, mode 1016) and bracketed paste.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Tab,
    Backspace,
    Esc,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    F(u8),
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Mods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

impl Mods {
    /// Decodes the `1 + bitmask` modifier parameter of CSI sequences.
    fn from_param(p: u32) -> Mods {
        let bits = p.saturating_sub(1);
        Mods { shift: bits & 1 != 0, alt: bits & 2 != 0, ctrl: bits & 4 != 0 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    Press(Button),
    Release(Button),
    Drag(Button),
    /// Horizontal and vertical steps (right and up are positive)
    Scroll(i8, i8),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Key(Key, Mods),
    Mouse(MouseKind, i32, i32),
    Paste(String),
}

#[derive(Default)]
pub struct Parser {
    buf: Vec<u8>,
    in_paste: bool,
}

const PASTE_END: &[u8] = b"\x1b[201~";

impl Parser {
    pub fn feed(&mut self, bytes: &[u8], events: &mut Vec<Event>) {
        self.buf.extend_from_slice(bytes);
        self.parse(false, events);
    }

    /// Called when no more input arrived for a while: whatever is pending is
    /// complete (e.g. a lone ESC is the Escape key).
    pub fn flush(&mut self, events: &mut Vec<Event>) {
        self.parse(true, events);
    }

    fn parse(&mut self, eof: bool, events: &mut Vec<Event>) {
        let mut i = 0;
        while i < self.buf.len() {
            let rest = &self.buf[i..];
            if self.in_paste {
                let Some(end) = rest.windows(PASTE_END.len()).position(|w| w == PASTE_END) else {
                    break;
                };
                events.push(Event::Paste(String::from_utf8_lossy(&rest[..end]).into_owned()));
                self.in_paste = false;
                i += end + PASTE_END.len();
                continue;
            }
            match parse_one(rest, eof) {
                None => break,
                Some((n, ev)) => {
                    i += n;
                    match ev {
                        Some(Parsed::Event(e)) => events.push(e),
                        Some(Parsed::PasteStart) => self.in_paste = true,
                        None => {}
                    }
                }
            }
        }
        self.buf.drain(..i);
        if eof && !self.in_paste {
            self.buf.clear();
        }
    }
}

enum Parsed {
    Event(Event),
    PasteStart,
}

fn key(k: Key, mods: Mods) -> Option<Parsed> {
    Some(Parsed::Event(Event::Key(k, mods)))
}

/// Parses one token. Returns `None` if more bytes are needed.
fn parse_one(b: &[u8], eof: bool) -> Option<(usize, Option<Parsed>)> {
    let none = Mods::default();
    match b[0] {
        0x1b => {
            let Some(&next) = b.get(1) else {
                return eof.then(|| (1, key(Key::Esc, none)));
            };
            match next {
                b'[' => parse_csi(b, eof),
                b'O' => match b.get(2) {
                    None => eof.then_some((b.len(), None)),
                    Some(&f) => Some((3, ss3_key(f).and_then(|k| key(k, none)))),
                },
                b'_' | b']' | b'P' => skip_string(b, eof),
                _ => {
                    // Legacy Alt+key
                    let (n, ev) = parse_plain(&b[1..], eof)?;
                    let ev = match ev {
                        Some(Parsed::Event(Event::Key(k, m))) => key(k, Mods { alt: true, ..m }),
                        other => other,
                    };
                    Some((n + 1, ev))
                }
            }
        }
        _ => parse_plain(b, eof),
    }
}

fn parse_plain(b: &[u8], eof: bool) -> Option<(usize, Option<Parsed>)> {
    let none = Mods::default();
    let ctrl = Mods { ctrl: true, ..none };
    let c = b[0];
    let ev = match c {
        b'\r' | b'\n' => key(Key::Enter, none),
        b'\t' => key(Key::Tab, none),
        0x7f | 0x08 => key(Key::Backspace, none),
        0x00 => key(Key::Char(' '), ctrl),
        0x01..=0x1a => key(Key::Char((b'a' + c - 1) as char), ctrl),
        0x1b => key(Key::Esc, none),
        0x1c..=0x1f => None,
        _ => {
            let len = match c {
                0x00..=0x7f => 1,
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf7 => 4,
                _ => return Some((1, None)),
            };
            if b.len() < len {
                return eof.then_some((b.len(), None));
            }
            let ch = std::str::from_utf8(&b[..len]).ok().and_then(|s| s.chars().next());
            return Some((len, ch.and_then(|ch| key(Key::Char(ch), none))));
        }
    };
    Some((1, ev))
}

fn parse_csi(b: &[u8], eof: bool) -> Option<(usize, Option<Parsed>)> {
    let Some(end) = b[2..].iter().position(|c| (0x40..=0x7e).contains(c)).map(|p| p + 2) else {
        return eof.then_some((b.len(), None));
    };
    let params = std::str::from_utf8(&b[2..end]).unwrap_or("");
    let fin = b[end];
    let n = end + 1;

    if let Some(p) = params.strip_prefix('<') {
        return Some((n, parse_sgr_mouse(p, fin)));
    }

    // Parameters are `;`-separated, each with optional `:` sub-parameters
    let field = |i: usize| -> Option<u32> { params.split(';').nth(i)?.split(':').next()?.parse().ok() };
    let mods = Mods::from_param(field(1).unwrap_or(1));

    let k = match fin {
        b'u' => field(0).and_then(kitty_key),
        b'~' if field(0) == Some(200) => return Some((n, Some(Parsed::PasteStart))),
        b'~' => field(0).and_then(tilde_key),
        f => ss3_key(f),
    };
    Some((n, k.and_then(|k| key(k, mods))))
}

fn kitty_key(code: u32) -> Option<Key> {
    Some(match code {
        27 => Key::Esc,
        13 => Key::Enter,
        9 => Key::Tab,
        127 => Key::Backspace,
        // Private use area: keypad, media and modifier keys
        57344..=63743 => return None,
        _ => Key::Char(char::from_u32(code)?),
    })
}

fn tilde_key(code: u32) -> Option<Key> {
    Some(match code {
        1 | 7 => Key::Home,
        2 => Key::Insert,
        3 => Key::Delete,
        4 | 8 => Key::End,
        5 => Key::PageUp,
        6 => Key::PageDown,
        11..=15 => Key::F((code - 10) as u8),
        17..=21 => Key::F((code - 11) as u8),
        23..=24 => Key::F((code - 12) as u8),
        _ => return None,
    })
}

fn ss3_key(f: u8) -> Option<Key> {
    Some(match f {
        b'A' => Key::Up,
        b'B' => Key::Down,
        b'C' => Key::Right,
        b'D' => Key::Left,
        b'H' => Key::Home,
        b'F' => Key::End,
        b'P' => Key::F(1),
        b'Q' => Key::F(2),
        b'R' => Key::F(3),
        b'S' => Key::F(4),
        _ => return None,
    })
}

fn parse_sgr_mouse(params: &str, fin: u8) -> Option<Parsed> {
    let mut it = params.split(';').map(|s| s.parse::<i32>().ok());
    let (cb, x, y) = (it.next()??, it.next()??, it.next()??);
    // Coordinates are 1-based
    let (x, y) = ((x - 1).max(0), (y - 1).max(0));

    if cb & 64 != 0 {
        let (h, v) = match cb & 3 {
            0 => (0, 1),
            1 => (0, -1),
            2 => (-1, 0),
            _ => (1, 0),
        };
        return Some(Parsed::Event(Event::Mouse(MouseKind::Scroll(h, v), x, y)));
    }
    if cb & 128 != 0 {
        return None; // extra buttons
    }
    let button = match cb & 3 {
        0 => Button::Left,
        1 => Button::Middle,
        2 => Button::Right,
        _ => return None, // motion without button
    };
    let kind = match (fin, cb & 32 != 0) {
        (b'm', _) => MouseKind::Release(button),
        (_, true) => MouseKind::Drag(button),
        _ => MouseKind::Press(button),
    };
    Some(Parsed::Event(Event::Mouse(kind, x, y)))
}

/// Skips APC/OSC/DCS strings (e.g. kitty graphics replies).
fn skip_string(b: &[u8], eof: bool) -> Option<(usize, Option<Parsed>)> {
    for i in 2..b.len() {
        if b[i] == 0x07 {
            return Some((i + 1, None));
        }
        if b[i] == 0x1b && b.get(i + 1) == Some(&b'\\') {
            return Some((i + 2, None));
        }
    }
    eof.then_some((b.len(), None))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(bytes: &[u8]) -> Vec<Event> {
        let mut p = Parser::default();
        let mut ev = Vec::new();
        p.feed(bytes, &mut ev);
        p.flush(&mut ev);
        ev
    }

    #[test]
    fn text_and_controls() {
        let none = Mods::default();
        assert_eq!(
            parse("a中\r\x7f".as_bytes()),
            vec![
                Event::Key(Key::Char('a'), none),
                Event::Key(Key::Char('中'), none),
                Event::Key(Key::Enter, none),
                Event::Key(Key::Backspace, none),
            ]
        );
    }

    #[test]
    fn kitty_keys() {
        let ctrl = Mods { ctrl: true, ..Default::default() };
        let alt_shift = Mods { alt: true, shift: true, ..Default::default() };
        assert_eq!(parse(b"\x1b[113;5u"), vec![Event::Key(Key::Char('q'), ctrl)]);
        assert_eq!(parse(b"\x1b[110;4u"), vec![Event::Key(Key::Char('n'), alt_shift)]);
        assert_eq!(parse(b"\x1b[27u"), vec![Event::Key(Key::Esc, Mods::default())]);
        assert_eq!(parse(b"\x1b[1;5A"), vec![Event::Key(Key::Up, ctrl)]);
    }

    #[test]
    fn split_sequences() {
        let mut p = Parser::default();
        let mut ev = Vec::new();
        p.feed(b"\x1b[<0;10", &mut ev);
        assert!(ev.is_empty());
        p.feed(b";20M", &mut ev);
        assert_eq!(ev, vec![Event::Mouse(MouseKind::Press(Button::Left), 9, 19)]);
    }

    #[test]
    fn mouse() {
        assert_eq!(
            parse(b"\x1b[<32;5;6M\x1b[<0;5;6m\x1b[<65;1;1M"),
            vec![
                Event::Mouse(MouseKind::Drag(Button::Left), 4, 5),
                Event::Mouse(MouseKind::Release(Button::Left), 4, 5),
                Event::Mouse(MouseKind::Scroll(0, -1), 0, 0),
            ]
        );
    }

    #[test]
    fn paste_and_apc() {
        assert_eq!(parse(b"\x1b_Gi=1;OK\x1b\\\x1b[200~hi\x1b[201~"), vec![Event::Paste("hi".into())]);
    }

    #[test]
    fn lone_escape() {
        assert_eq!(parse(b"\x1b"), vec![Event::Key(Key::Esc, Mods::default())]);
    }
}
