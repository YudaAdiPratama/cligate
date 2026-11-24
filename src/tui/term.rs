use std::io::{self, Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

pub(super) struct RawTermGuard {
    saved_state: Option<String>,
}

impl RawTermGuard {
    pub(super) fn enter() -> Self {
        let saved = Command::new("stty")
            .arg("-g")
            .stdin(Stdio::inherit())
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                } else {
                    None
                }
            });
        let _ = Command::new("stty")
            .args([
                "-icanon", "-echo", "-isig", "-ixon", "min", "1", "time", "0",
            ])
            .stdin(Stdio::inherit())
            .status();
        let mut out = io::stdout();
        let _ =
            out.write_all(b"\x1b[?1049h\x1b[?1007l\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[2J\x1b[H");
        let _ = out.flush();
        Self { saved_state: saved }
    }
}

impl Drop for RawTermGuard {
    fn drop(&mut self) {
        let mut out = io::stdout();
        let _ = out
            .write_all(b"\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[?1007l\x1b[?25h\x1b[0m\x1b[?1049l");
        let _ = out.flush();
        let restore = match self.saved_state {
            Some(ref st) => Command::new("stty")
                .arg(st)
                .stdin(Stdio::inherit())
                .status(),
            None => Command::new("stty")
                .arg("sane")
                .stdin(Stdio::inherit())
                .status(),
        };
        let _ = restore;
    }
}

fn parse_stty_size(out: &std::process::Output) -> Option<(usize, usize)> {
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let mut parts = s.split_whitespace();
    let r = parts.next()?.parse::<usize>().ok()?;
    let c = parts.next()?.parse::<usize>().ok()?;
    if r >= 10 && c >= 30 {
        Some((r, c))
    } else {
        None
    }
}

pub(super) fn detect_term_size() -> (usize, usize) {
    let probes: &[&[&str]] = &[
        &["-F", "/dev/tty", "size"],
        &["-f", "/dev/tty", "size"],
        &["size"],
    ];
    for args in probes {
        if let Ok(out) = Command::new("stty")
            .args(*args)
            .stdin(Stdio::inherit())
            .stderr(Stdio::null())
            .output()
        {
            if let Some(sz) = parse_stty_size(&out) {
                return sz;
            }
        }
    }
    if let (Ok(r), Ok(c)) = (std::env::var("LINES"), std::env::var("COLUMNS")) {
        if let (Ok(rows), Ok(cols)) = (r.parse::<usize>(), c.parse::<usize>()) {
            if rows >= 10 && cols >= 30 {
                return (rows, cols);
            }
        }
    }
    (28, 100)
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub(super) enum Key {
    Char(char),
    Enter,
    Tab,
    Backspace,
    Delete,
    Esc,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    ScrollUp,
    ScrollDown,
    F2,
    F3,
    F4,
    F5,
    CtrlC,
    CtrlG,
    CtrlL,
    CtrlN,
    CtrlP,
    CtrlR,
    CtrlT,
    CtrlU,
    Unknown,
}

pub(super) fn spawn_input_thread() -> Receiver<u8> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut b = [0u8; 1];
        while let Ok(1) = stdin.read(&mut b) {
            if tx.send(b[0]).is_err() {
                break;
            }
        }
    });
    rx
}

fn parse_escape(rx: &Receiver<u8>) -> Key {
    let short = Duration::from_millis(40);
    let Ok(b1) = rx.recv_timeout(short) else {
        return Key::Esc;
    };
    match b1 {
        b'[' => {
            let mut params = String::new();
            loop {
                let Ok(b) = rx.recv_timeout(short) else {
                    return Key::Unknown;
                };
                if (0x40..=0x7e).contains(&b) {
                    if b == b'M' && params.is_empty() {
                        let cb = rx.recv_timeout(short).ok().unwrap_or(0);
                        let _ = rx.recv_timeout(short);
                        let _ = rx.recv_timeout(short);
                        return match cb.saturating_sub(32) & 0x43 {
                            64 => Key::ScrollUp,
                            65 => Key::ScrollDown,
                            _ => Key::Unknown,
                        };
                    }
                    if matches!(b, b'M' | b'm') && params.starts_with('<') {
                        if let Some(btn) = params[1..]
                            .split(';')
                            .next()
                            .and_then(|s| s.parse::<u32>().ok())
                        {
                            return match btn & 0x43 {
                                64 => Key::ScrollUp,
                                65 => Key::ScrollDown,
                                _ => Key::Unknown,
                            };
                        }
                        return Key::Unknown;
                    }
                    return match (b as char, params.as_str()) {
                        ('A', p) if p.contains(';') => Key::ScrollUp,
                        ('B', p) if p.contains(';') => Key::ScrollDown,
                        ('A', _) => Key::Up,
                        ('B', _) => Key::Down,
                        ('C', _) => Key::Right,
                        ('D', _) => Key::Left,
                        ('H', _) | ('~', "1") | ('~', "7") => Key::Home,
                        ('F', _) | ('~', "4") | ('~', "8") => Key::End,
                        ('~', "3") => Key::Delete,
                        ('~', "5") => Key::PageUp,
                        ('~', "6") => Key::PageDown,
                        ('~', "12") => Key::F2,
                        ('~', "13") => Key::F3,
                        ('~', "14") => Key::F4,
                        ('~', "15") => Key::F5,
                        _ => Key::Unknown,
                    };
                }
                params.push(b as char);
            }
        }
        b'O' => match rx.recv_timeout(short) {
            Ok(b'Q') => Key::F2,
            Ok(b'R') => Key::F3,
            Ok(b'S') => Key::F4,
            Ok(b'H') => Key::Home,
            Ok(b'F') => Key::End,
            _ => Key::Unknown,
        },
        _ => Key::Esc,
    }
}

fn decode_key(first: u8, rx: &Receiver<u8>) -> Key {
    match first {
        3 | 4 => Key::CtrlC,
        1 => Key::Home,
        5 => Key::End,
        7 => Key::CtrlG,
        9 => Key::Tab,
        10 | 13 => Key::Enter,
        12 => Key::CtrlL,
        14 => Key::CtrlN,
        16 => Key::CtrlP,
        18 => Key::CtrlR,
        20 => Key::CtrlT,
        21 => Key::CtrlU,
        27 => parse_escape(rx),
        127 | 8 => Key::Backspace,
        c if (32..127).contains(&c) => Key::Char(c as char),
        lead if lead >= 0xC0 => {
            let extra = if lead < 0xE0 {
                1
            } else if lead < 0xF0 {
                2
            } else {
                3
            };
            let mut buf = vec![lead];
            for _ in 0..extra {
                match rx.recv_timeout(Duration::from_millis(40)) {
                    Ok(b) => buf.push(b),
                    Err(_) => break,
                }
            }
            std::str::from_utf8(&buf)
                .ok()
                .and_then(|s| s.chars().next())
                .map(Key::Char)
                .unwrap_or(Key::Unknown)
        }
        _ => Key::Unknown,
    }
}

pub(super) fn next_key(rx: &Receiver<u8>, wait: Duration) -> Option<Key> {
    match rx.recv_timeout(wait) {
        Ok(b) => Some(decode_key(b, rx)),
        Err(RecvTimeoutError::Timeout) => None,
        Err(RecvTimeoutError::Disconnected) => Some(Key::CtrlC),
    }
}
