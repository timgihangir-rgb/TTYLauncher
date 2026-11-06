//! Example 2: show raw terminal input — keys and mouse events.
//!
//! Enables raw mode + SGR mouse reporting, prints every decoded event.
//! This is how the launcher will capture player input in game mode.
//!
//! Run:  cargo run --example term_events
//! Quit: Ctrl+C or q

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind,
};
use crossterm::{cursor, execute, terminal};
use std::io;
use std::time::Duration;

fn main() -> io::Result<()> {
    terminal::enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, terminal::EnterAlternateScreen, EnableMouseCapture, cursor::Hide)?;

    println!("Raw input demo. Move/click the mouse, press keys. q or Ctrl+C quits.\r");

    loop {
        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => {
                    let mods = format!("{:?}", k.modifiers);
                    println!("KEY {:?} mods={mods}\r", k.code);
                    if k.code == KeyCode::Char('q') || k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
                        break;
                    }
                }
                Event::Mouse(m) => {
                    let kind = match m.kind {
                        MouseEventKind::Down(b) => format!("down({b:?})"),
                        MouseEventKind::Up(b) => format!("up({b:?})"),
                        MouseEventKind::Drag(b) => format!("drag({b:?})"),
                        MouseEventKind::Moved => "moved".into(),
                        MouseEventKind::ScrollDown => "wheel down".into(),
                        MouseEventKind::ScrollUp => "wheel up".into(),
                        _ => format!("{:?}", m.kind),
                    };
                    println!(
                        "MOUSE {kind} at col={} row={} (buttons: L={} M={} R={})\r",
                        m.column, m.row,
                        matches!(m.kind, MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left)),
                        matches!(m.kind, MouseEventKind::Down(MouseButton::Middle) | MouseEventKind::Up(MouseButton::Middle)),
                        matches!(m.kind, MouseEventKind::Down(MouseButton::Right) | MouseEventKind::Up(MouseButton::Right)),
                    );
                }
                Event::Resize(w, h) => println!("RESIZE {w}x{h}\r"),
                _ => {}
            }
        }
    }

    execute!(out, EnableMouseCapture, DisableMouseCapture)?;
    execute!(out, terminal::LeaveAlternateScreen, cursor::Show)?;
    terminal::disable_raw_mode()?;
    Ok(())
}
