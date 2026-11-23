//! `readgate attach`: answers the running readgate's questions from a
//! terminal. It waits, and when a read comes in it rings the bell and
//! shows the question; a key answers it. Whichever answers first, this
//! terminal, another one or the app's window, wins, and the others stop
//! showing it.

use std::io::{BufRead, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Print, SetAttribute};
use crossterm::terminal::{self, ClearType};
use crossterm::{cursor, execute, queue};

use crate::remote::{Client, Kind, ToClient, ToServer, WireAnswer, WireQuestion};
use crate::service::countdown;

/// How often the countdown is redrawn, and the keyboard checked.
const TICK: Duration = Duration::from_millis(250);

/// How long a new question ignores keys, so a key meant for the one
/// before (pressed twice, or repeating) can't answer it unread.
const GRACE: Duration = Duration::from_millis(750);

struct Asking {
    question: WireQuestion,
    deadline: Instant,
    /// When it came in, for the grace.
    shown: Instant,
}

pub fn run() -> Result<()> {
    let mut client = Client::connect()?;
    client.send(&ToServer::Attach)?;
    let to = match client.kind {
        Kind::App => "the readgate app",
        Kind::Serve => "readgate serve",
    };
    let (reader, mut writer) = client.into_parts();
    // `None` once readgate is gone.
    let (messages, received) = mpsc::channel();
    std::thread::spawn(move || {
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if let Ok(message) = serde_json::from_str::<ToClient>(&line) {
                let _ = messages.send(Some(message));
            }
        }
        let _ = messages.send(None);
    });

    let mut screen = Screen::enter()?;
    let mut asking: Option<Asking> = None;
    loop {
        loop {
            match received.try_recv() {
                Ok(Some(ToClient::Ask(question))) => {
                    let deadline = Instant::now() + Duration::from_secs_f64(question.seconds_left.max(0.0));
                    asking = Some(Asking { question, deadline, shown: Instant::now() });
                    // Keys pressed before it showed weren't for it.
                    while event::poll(Duration::ZERO)? {
                        event::read()?;
                    }
                    screen.bell()?;
                }
                Ok(Some(ToClient::Done { id })) => {
                    if asking.as_ref().is_some_and(|a| a.question.id == id) {
                        asking = None;
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(mpsc::TryRecvError::Disconnected) => {
                    drop(screen);
                    bail!("readgate stopped");
                }
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        screen.draw(to, asking.as_ref())?;
        if !event::poll(TICK)? {
            continue;
        }
        let key = match event::read()? {
            Event::Key(key) => key,
            Event::Resize(..) => {
                screen.invalidate();
                continue;
            }
            _ => continue,
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let quit = matches!(key.code, KeyCode::Char('q'))
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL));
        if quit {
            return Ok(());
        }
        let Some(a) = &asking else { continue };
        if a.shown.elapsed() < GRACE {
            continue;
        }
        let q = &a.question;
        let answer = match key.code {
            KeyCode::Char('a') => WireAnswer::Allow,
            KeyCode::Char('d') | KeyCode::Esc => WireAnswer::Deny,
            KeyCode::Char('m') if q.grant.is_some() => WireAnswer::Minute,
            KeyCode::Char(c @ '1'..='9') => {
                let n = c as usize - '1' as usize;
                match q.grant_choices.get(n) {
                    Some(choice) => WireAnswer::MinuteTo { at: choice.at },
                    None => continue,
                }
            }
            _ => continue,
        };
        let mut line = serde_json::to_vec(&ToServer::Answer { id: q.id, answer })?;
        line.push(b'\n');
        writer.write_all(&line)?;
        // Readgate's done follows; until then, nothing to answer.
        asking = None;
    }
}

/// The terminal in raw mode on its alternate screen, given back as it was
/// when dropped.
struct Screen {
    /// What was drawn last, so an unchanged frame isn't drawn again.
    last: Vec<u8>,
}

impl Screen {
    fn enter() -> Result<Screen> {
        terminal::enable_raw_mode()?;
        execute!(std::io::stdout(), terminal::EnterAlternateScreen, cursor::Hide)?;
        Ok(Screen { last: Vec::new() })
    }

    fn bell(&self) -> Result<()> {
        let mut out = std::io::stdout();
        out.write_all(b"\x07")?;
        out.flush()?;
        Ok(())
    }

    fn draw(&mut self, to: &str, asking: Option<&Asking>) -> Result<()> {
        let (width, height) = terminal::size()?;
        let (width, height) = (usize::from(width.max(20)), usize::from(height.max(5)));
        let mut out = Vec::new();
        queue!(out, terminal::Clear(ClearType::All), cursor::MoveTo(0, 0))?;
        let mut line = |text: &str, attribute: Option<Attribute>| -> Result<()> {
            if let Some(attribute) = attribute {
                queue!(out, SetAttribute(attribute))?;
            }
            queue!(out, Print(plain(text)), SetAttribute(Attribute::Reset), Print("\r\n"))?;
            Ok(())
        };
        line(&format!("readgate, attached to {to}"), Some(Attribute::Dim))?;
        line("", None)?;
        let Some(a) = asking else {
            line("Waiting for reads. q to detach.", None)?;
            return self.show(out);
        };
        let q = &a.question;
        let left = a.deadline.saturating_duration_since(Instant::now());
        let seconds = left.as_secs() + u64::from(left.subsec_nanos() > 0);

        let mut keys = vec![String::from("[a] Allow   [d] Deny")];
        if let Some(parent) = &q.grant {
            keys.push(format!("[m] Allow {parent} for 1 Minute"));
        }
        for (n, choice) in q.grant_choices.iter().take(9).enumerate() {
            keys.push(format!("[{}] Allow {} for 1 Minute", n + 1, choice.label));
        }
        keys.push(String::from("[q] Detach, leaving the question to the others"));

        // What's left for the details, between the title and the keys.
        let rows = |text: &str| plain(text).chars().count().div_ceil(width).max(1);
        let fixed = 2 + rows(&q.title) + 1 + 1 + 1 + keys.iter().map(|k| rows(k)).sum::<usize>();
        let mut room = height.saturating_sub(fixed);

        line(&q.title, Some(Attribute::Bold))?;
        let details: Vec<&str> = q.details.lines().collect();
        for (i, detail) in details.iter().enumerate() {
            let needed = rows(detail);
            // Keep a row for saying the rest is cut.
            let last = i + 1 == details.len();
            if needed > room || (!last && needed == room) {
                line("… (the rest doesn't fit; make the terminal taller)", Some(Attribute::Dim))?;
                break;
            }
            room -= needed;
            line(detail, None)?;
        }
        line("", None)?;
        line(&format!("Denies in {}", countdown(seconds)), Some(Attribute::Dim))?;
        for key in &keys {
            line(key, None)?;
        }
        self.show(out)
    }

    fn show(&mut self, frame: Vec<u8>) -> Result<()> {
        if frame != self.last {
            let mut out = std::io::stdout();
            out.write_all(&frame)?;
            out.flush()?;
            self.last = frame;
        }
        Ok(())
    }

    /// Draws the next frame even if it's the same, as after a resize.
    fn invalidate(&mut self) {
        self.last.clear();
    }
}

/// `text` with nothing in it that the terminal would act on. Most of what
/// a question shows comes from the reader, whose user picks its program's
/// name and command line: an escape sequence in them could redraw the
/// screen and make the question say something else. Control characters
/// show as their Control Pictures (`␛`), and bidi overrides, which could
/// turn text around, as `�`.
fn plain(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\t' => ' ',
            '\u{0}'..='\u{1f}' => char::from_u32(0x2400 + c as u32).unwrap_or('\u{fffd}'),
            '\u{7f}' => '\u{2421}',
            c if c.is_control() => '\u{fffd}',
            '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => '\u{fffd}',
            c => c,
        })
        .collect()
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = execute!(std::io::stdout(), cursor::Show, terminal::LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}
