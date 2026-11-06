//! The launcher dashboard: full-screen framed TUI with a version browser,
//! a details panel, settings and a live status bar.

use std::io::{self, Write};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::style::Color;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{cursor, execute, queue};

use crate::config::Config;
use crate::display;
use crate::mc::{self, VersionEntry};
use crate::session;

struct Menu {
    config: Config,
    remote: Vec<VersionEntry>,
    /// merged list: (name, type, installed, source)
    items: Vec<Item>,
    filtered: Vec<usize>,
    sel: usize,
    /// first filtered row shown in the list; kept independent of `sel` so a
    /// click or a step up/down does not re-centre (and jump) the whole list
    scroll: usize,
    filter: String,
    status: String,
    mode: Mode,
    edit_buf: String,
    /// detail panel for the selected version: type, installed
    detail: Option<Item>,
}

#[derive(PartialEq)]
enum Mode {
    Browse,
    Filter,
    EditUsername,
    EditXmx,
    EditFps,
}

#[derive(Clone)]
struct Item {
    name: String,
    vtype: String,
    installed: bool,
}

/// Enter the launcher TUI (raw mode + alternate screen + hidden cursor).
///
/// The menu must *release* the terminal before a game session and take it
/// back afterwards: the session sets up (and tears down) the same state, and
/// a nested `disable_raw_mode()` would otherwise leave the menu in cooked mode
/// where keys only arrive after Enter.
fn tui_enter() -> Result<()> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    // Clicking a version is how most people pick one; without mouse capture
    // the click silently does nothing and Enter then launches whatever was
    // selected before.
    execute!(out, EnterAlternateScreen, cursor::Hide, EnableMouseCapture)?;
    Ok(())
}

fn tui_leave() -> Result<()> {
    let mut out = io::stdout();
    execute!(out, DisableMouseCapture, cursor::Show, LeaveAlternateScreen)?;
    disable_raw_mode()?;
    Ok(())
}

// --------------------------------------------------------------------- text

/// A styled run of text. Lines are built from spans and truncated by their
/// visible (character) width, so ANSI sequences never break the layout.
#[derive(Clone)]
struct Span {
    text: String,
    fg: Option<Color>,
    bg: Option<Color>,
    bold: bool,
}

impl Span {
    fn plain(text: impl Into<String>) -> Self {
        Self { text: text.into(), fg: None, bg: None, bold: false }
    }
    fn fg(mut self, c: Color) -> Self {
        self.fg = Some(c);
        self
    }
    fn bg(mut self, c: Color) -> Self {
        self.bg = Some(c);
        self
    }
    fn bold(mut self) -> Self {
        self.bold = true;
        self
    }
    fn dim(mut self) -> Self {
        self.fg = Some(Color::DarkGrey);
        self
    }
}

/// Render spans into one terminal line, clipped to `max` visible columns.
fn render(spans: &[Span], max: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for span in spans {
        if used >= max {
            break;
        }
        let mut text = String::new();
        for ch in span.text.chars() {
            if used >= max {
                break;
            }
            text.push(ch);
            used += 1;
        }
        if text.is_empty() {
            continue;
        }
        let mut styled = String::new();
        if span.bold {
            styled.push_str("\x1b[1m");
        }
        if let Some(fg) = span.fg {
            styled.push_str(&format!("\x1b[38;5;{}m", fg_code(fg)));
        }
        if let Some(bg) = span.bg {
            styled.push_str(&format!("\x1b[48;5;{}m", fg_code(bg)));
        }
        out.push_str(&styled);
        out.push_str(&text);
        out.push_str("\x1b[0m");
    }
    out
}

fn fg_code(c: Color) -> u8 {
    match c {
        Color::Black => 0,
        Color::DarkRed => 1,
        Color::DarkGreen => 2,
        Color::DarkYellow => 3,
        Color::DarkBlue => 4,
        Color::DarkMagenta => 5,
        Color::DarkCyan => 6,
        Color::Grey => 7,
        Color::DarkGrey => 8,
        Color::Red => 9,
        Color::Green => 10,
        Color::Yellow => 11,
        Color::Blue => 12,
        Color::Magenta => 13,
        Color::Cyan => 14,
        Color::White => 15,
        _ => 7,
    }
}

/// Pad a list of spans with spaces to exactly `width` visible columns.
fn pad_to(mut spans: Vec<Span>, width: usize) -> Vec<Span> {
    let used: usize = spans.iter().map(|s| s.text.chars().count()).sum();
    if used < width {
        spans.push(Span::plain(" ".repeat(width - used)));
    }
    spans
}

fn type_color(vtype: &str) -> Color {
    match vtype {
        "release" => Color::Green,
        "snapshot" => Color::Yellow,
        "old_beta" | "old_alpha" => Color::DarkYellow,
        "local" => Color::Cyan,
        _ => Color::Grey,
    }
}

// --------------------------------------------------------------------- menu

/// Rows of the version list that fit on the current terminal.
fn visible_rows() -> usize {
    let (_, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let box_h = (rows as usize).saturating_sub(6).max(3);
    box_h.saturating_sub(2)
}

/// Which filtered-list position sits under a terminal cell.
///
/// Mirrors the layout `draw()` produces: the header takes row 0, the panel
/// border row 1, versions start at row 2, and the left panel starts after
/// `" " + border + " "` (column 3). `None` when the cell is not a version row.
fn list_cell(
    cols: usize,
    rows: usize,
    col: usize,
    row: usize,
    scroll: usize,
    len: usize,
) -> Option<usize> {
    // below this size draw() switches to the compact layout without a list
    if cols < 52 || rows < 16 {
        return None;
    }
    let box_h = rows.saturating_sub(6).max(3);
    let list_h = box_h.saturating_sub(2);
    let vis = (row as i32) - 2;
    if vis < 0 || vis as usize >= list_h {
        return None;
    }
    // same split as draw(): the left panel is `lw` columns wide
    let lw = (((cols - 5) as f32 * 0.46) as usize).clamp(28, (cols - 5).saturating_sub(26));
    if col < 3 || col >= 3 + lw {
        return None;
    }
    let start = scroll.min(len.saturating_sub(list_h));
    let pos = start + vis as usize;
    (pos < len).then_some(pos)
}

impl Menu {
    fn new(config: Config) -> Self {
        let mut m = Self {
            config,
            remote: Vec::new(),
            items: Vec::new(),
            filtered: Vec::new(),
            sel: 0,
            scroll: 0,
            filter: String::new(),
            status: String::new(),
            mode: Mode::Browse,
            edit_buf: String::new(),
            detail: None,
        };
        m.reload();
        m
    }

    fn reload(&mut self) {
        self.remote = match load_manifest(&self.config) {
            Ok(m) => m,
            Err(e) => {
                self.status = format!("manifest unavailable: {e} (offline?)");
                Vec::new()
            }
        };
        let local = mc::local_versions(&self.config.game_dir);
        self.items.clear();
        for v in &self.remote {
            self.items.push(Item {
                name: v.id.clone(),
                vtype: v.r#type.clone(),
                installed: local.contains(&v.id),
            });
        }
        // local-only versions (e.g. Forge) that the remote manifest lacks
        for name in local {
            if !self.items.iter().any(|i| i.name == name) {
                self.items.push(Item {
                    name,
                    vtype: "local".into(),
                    installed: true,
                });
            }
        }
        // preselect last played
        if let Some(last) = &self.config.last_version
            && let Some(pos) = self.items.iter().position(|i| &i.name == last)
        {
            self.sel = pos;
        }
        self.refilter();
        // start with the last played version roughly in the middle
        self.scroll = self.sel.saturating_sub(visible_rows() / 2);
    }

    fn refilter(&mut self) {
        let current = self.filtered.get(self.sel).copied();
        let f = self.filter.to_lowercase();
        self.filtered = (0..self.items.len())
            .filter(|&i| self.items[i].name.to_lowercase().contains(&f))
            .collect();
        // keep the same version selected when it survives the filter
        self.sel = match current.and_then(|idx| self.filtered.iter().position(|&i| i == idx)) {
            Some(pos) => pos,
            None => self.sel.min(self.filtered.len().saturating_sub(1)),
        };
        self.scroll_into_view(visible_rows());
    }

    /// Scroll just enough to keep the selection visible.
    fn scroll_into_view(&mut self, list_h: usize) {
        if list_h == 0 {
            return;
        }
        if self.sel < self.scroll {
            self.scroll = self.sel;
        } else if self.sel >= self.scroll + list_h {
            self.scroll = self.sel + 1 - list_h;
        }
        let max = self.filtered.len().saturating_sub(list_h);
        self.scroll = self.scroll.min(max);
    }

    fn selected(&self) -> Option<&Item> {
        self.filtered.get(self.sel).map(|&i| &self.items[i])
    }

    fn refresh_detail(&mut self) {
        self.detail = self.selected().cloned();
    }

    // ------------------------------------------------------------- drawing

    fn draw(&self) -> Result<()> {
        let (w, h) = crossterm::terminal::size().unwrap_or((80, 24));
        let (w, h) = (w as usize, h as usize);
        let mut out = io::stdout();
        queue!(out, Clear(ClearType::All), cursor::Hide)?;

        if w < 52 || h < 16 {
            self.draw_tiny(&mut out, w, h)?;
            out.flush()?;
            return Ok(());
        }

        let mut frame: Vec<Vec<Span>> = Vec::with_capacity(h);
        frame.push(self.header_line(w));
        // two inner panels
        let lw = ((w - 5) as f32 * 0.46) as usize;
        let lw = lw.clamp(28, (w - 5).saturating_sub(26));
        let rw = (w - 5).saturating_sub(lw);
        let box_h = h.saturating_sub(6).max(3); // rows 2..h-4

        let left = self.left_panel(lw, box_h);
        let right = self.right_panel(rw, box_h);
        for i in 0..box_h {
            let mut spans = vec![Span::plain(" "), border_edge()];
            spans.push(Span::plain(" "));
            spans.extend(left.get(i).cloned().unwrap_or_default());
            spans.push(Span::plain(" "));
            spans.extend(right.get(i).cloned().unwrap_or_default());
            spans.push(Span::plain(" "));
            spans.push(border_edge());
            frame.push(spans);
        }
        frame.push(self.status_line(w));
        frame.push(self.footer_line(w));

        for (y, row) in frame.iter().enumerate() {
            if y >= h {
                break;
            }
            queue!(
                out,
                cursor::MoveTo(0, y as u16),
                crossterm::style::Print(render(row, w)),
            )?;
        }
        if !matches!(self.mode, Mode::Browse) {
            // put the caret at the end of the edit buffer
            let prompt = self.prompt();
            let col = 3 + prompt.chars().count() + self.edit_len();
            queue!(out, cursor::MoveTo(col.min(w.saturating_sub(1)) as u16, (h - 2) as u16), cursor::Show)?;
        }
        out.flush()?;
        Ok(())
    }

    fn draw_tiny(&self, out: &mut impl Write, w: usize, h: usize) -> Result<()> {
        let lines: [Vec<Span>; 6] = [
            vec![Span::plain("ttycraft").fg(Color::Magenta).bold()],
            vec![],
            vec![Span::plain("terminal too small").fg(Color::Yellow)],
            vec![Span::plain(format!("need at least 52x16, have {w}x{h}")).dim()],
            vec![],
            vec![Span::plain("q quits").dim()],
        ];
        for (i, line) in lines.iter().enumerate() {
            queue!(
                out,
                cursor::MoveTo(0, (h / 2).saturating_sub(3) as u16 + i as u16),
                crossterm::style::Print(render(line, w)),
            )?;
        }
        Ok(())
    }

    fn header_line(&self, w: usize) -> Vec<Span> {
        let title = " ttycraft ";
        let full = format!(
            " user: {}   mem: {} MB   fps: {}   res: {} ",
            self.config.username, self.config.xmx_mb, self.config.fps, self.config.resolution
        );
        let short = format!(" {} ", self.config.username);
        // pick the longest settings summary that still fits
        let right = if w >= 2 + title.chars().count() + full.chars().count() {
            full
        } else if w >= 2 + title.chars().count() + short.chars().count() {
            short
        } else {
            String::new()
        };
        let fill = w.saturating_sub(2 + title.chars().count() + right.chars().count());
        let mut spans = vec![
            Span::plain("╔").dim(),
            Span::plain(title).fg(Color::Magenta).bold(),
            Span::plain("═".repeat(fill)).dim(),
        ];
        if !right.is_empty() {
            spans.push(Span::plain(right).fg(Color::DarkCyan));
        }
        spans.push(Span::plain("╗").dim());
        spans
    }

    fn left_panel(&self, width: usize, height: usize) -> Vec<Vec<Span>> {
        let inner = width.saturating_sub(2);
        let title = if self.filter.is_empty() {
            format!(" Versions ({}) ", self.items.len())
        } else {
            format!(" Versions /{} ({}) ", self.filter, self.filtered.len())
        };
        let mut rows: Vec<Vec<Span>> = Vec::with_capacity(height);
        rows.push(box_top(&title, width));

        let list_h = height.saturating_sub(2);
        let max_scroll = self.filtered.len().saturating_sub(list_h);
        let start = self.scroll.min(max_scroll);
        let end = (start + list_h).min(self.filtered.len());
        for vis in 0..list_h {
            let idx = start + vis;
            if idx < end {
                let item = &self.items[self.filtered[idx]];
                let is_sel = idx == self.sel;
                let mut spans = vec![Span::plain(if is_sel { "▸ " } else { "  " })];
                let name_fg = if item.installed { Color::White } else { Color::Grey };
                spans.push(Span::plain(format!("{:<18}", item.name)).fg(name_fg));
                spans.push(Span::plain(" "));
                spans.push(
                    Span::plain(format!("{:<10}", item.vtype)).fg(type_color(&item.vtype)),
                );
                spans.push(Span::plain(" "));
                if item.installed {
                    spans.push(Span::plain("✓").fg(Color::Green));
                } else {
                    spans.push(Span::plain(" "));
                }
                let mut spans = pad_to(spans, inner);
                if is_sel {
                    spans = spans.into_iter().map(|s| s.bg(Color::DarkBlue).bold()).collect();
                }
                rows.push(prepend_bar(spans));
            } else {
                rows.push(empty_row(inner, None));
            }
        }
        rows.push(box_bottom(width));
        rows
    }

    fn right_panel(&self, width: usize, height: usize) -> Vec<Vec<Span>> {
        let inner = width.saturating_sub(2);
        let mut rows: Vec<Vec<Span>> = Vec::with_capacity(height);
        rows.push(box_top(" Details ", width));

        let mut body: Vec<Vec<Span>> = Vec::new();
        if let Some(item) = &self.detail {
            body.push(kv("Version", vec![Span::plain(item.name.clone()).fg(Color::White).bold()]));
            body.push(kv(
                "Type",
                vec![Span::plain(item.vtype.clone()).fg(type_color(&item.vtype))],
            ));
            body.push(kv(
                "State",
                if item.installed {
                    vec![Span::plain("installed ").fg(Color::Green), Span::plain("✓").fg(Color::Green)]
                } else {
                    vec![Span::plain("not installed").dim()]
                },
            ));
            if self.config.last_version.as_deref() == Some(item.name.as_str()) {
                body.push(kv("Last played", vec![Span::plain("yes").fg(Color::Cyan)]));
            }
            body.push(Vec::new());
        } else {
            body.push(vec![Span::plain("no version selected").dim()]);
            body.push(Vec::new());
        }

        body.push(vec![Span::plain(" Settings").fg(Color::Magenta).bold()]);
        body.push(kv("Username", vec![Span::plain(self.config.username.clone())]));
        body.push(kv("Memory", vec![Span::plain(format!("{} MB", self.config.xmx_mb))]));
        body.push(kv("Limit fps", vec![Span::plain(self.config.fps.to_string())]));
        body.push(kv(
            "Resolution",
            vec![Span::plain(self.config.resolution.clone())],
        ));
        body.push(kv(
            "Graphics",
            vec![Span::plain(if self.config.low_end {
                "fast (MC tuned)"
            } else {
                "vanilla"
            })],
        ));
        body.push(kv(
            "Edge mouse",
            vec![Span::plain(if self.config.mouse_recenter {
                "endless"
            } else {
                "limited"
            })],
        ));
        body.push(kv(
            "Game dir",
            vec![Span::plain(self.config.game_dir.display().to_string()).dim()],
        ));

        let list_h = height.saturating_sub(2);
        for i in 0..list_h {
            match body.get(i) {
                Some(spans) => {
                    let spans = pad_to(spans.clone(), inner);
                    rows.push(prepend_bar(spans));
                }
                None => rows.push(empty_row(inner, None)),
            }
        }
        rows.push(box_bottom(width));
        rows
    }

    fn status_line(&self, w: usize) -> Vec<Span> {
        let mut spans = vec![border_edge(), Span::plain(" ")];
        match self.mode {
            Mode::Browse => {
                let fg = if self.status.starts_with("error") {
                    Color::Red
                } else {
                    Color::Yellow
                };
                spans.push(Span::plain(self.status.clone()).fg(fg));
            }
            Mode::Filter => {
                spans.push(Span::plain("/").fg(Color::Cyan));
                spans.push(Span::plain(self.filter.clone()));
            }
            _ => {
                spans.push(Span::plain(self.prompt().trim_end()).fg(Color::Cyan));
                spans.push(Span::plain(self.edit_buf.clone()));
            }
        }
        let used: usize = spans.iter().map(|s| s.text.chars().count()).sum();
        let fill = w.saturating_sub(used + 1);
        spans.push(Span::plain(" ".repeat(fill)));
        spans.push(border_edge());
        spans
    }

    fn footer_line(&self, w: usize) -> Vec<Span> {
        let long = " ↑↓ move · Enter/click play · i install · / filter · n name · m memory · p fps · c resolution · g graphics · u mouse · r reload · q quit ";
        let short = " ↑↓/click · Enter · / filter · n m p c g u r · q quit ";
        let tiny = " q quit ";
        let help = match self.mode {
            Mode::Browse => {
                if long.chars().count() + 2 <= w {
                    long
                } else if short.chars().count() + 2 <= w {
                    short
                } else {
                    tiny
                }
            }
            _ => " Enter confirm · Esc cancel ",
        };
        let used = 1 + help.chars().count();
        let fill = w.saturating_sub(used + 1);
        vec![
            Span::plain("╚").dim(),
            Span::plain(help).dim(),
            Span::plain("═".repeat(fill)).dim(),
            Span::plain("╝").dim(),
        ]
    }

    fn prompt(&self) -> &'static str {
        match self.mode {
            Mode::Filter => "/",
            Mode::EditUsername => "username: ",
            Mode::EditXmx => "memory MB: ",
            Mode::EditFps => "fps: ",
            Mode::Browse => "",
        }
    }

    fn edit_len(&self) -> usize {
        match self.mode {
            Mode::Filter => self.filter.chars().count(),
            _ => self.edit_buf.chars().count(),
        }
    }

    // --------------------------------------------------------------- input

    /// Left click inside the version list. A click on a row selects it, a
    /// click on the already selected row activates it — the same as Enter.
    ///
    /// Without this, a menu user leaning on the mouse clicks a version,
    /// nothing visibly moves, and the next `Enter` still launches the
    /// previously selected ("last played") version.
    pub fn on_mouse(&mut self, m: MouseEvent) -> Action {
        if !matches!(self.mode, Mode::Browse | Mode::Filter) {
            return Action::None;
        }
        let (cols, rows) = crossterm::terminal::size().unwrap_or((0, 0));
        let (cols, rows) = (cols as usize, rows as usize);
        let list_h = rows.saturating_sub(6).max(3).saturating_sub(2);

        // the wheel scrolls the list without moving the selection
        match m.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                if cols < 52 || rows < 16 || list_h == 0 {
                    return Action::None;
                }
                let max = self.filtered.len().saturating_sub(list_h);
                let by = 3;
                self.scroll = if matches!(m.kind, MouseEventKind::ScrollUp) {
                    self.scroll.saturating_sub(by)
                } else {
                    (self.scroll + by).min(max)
                };
                return Action::Redraw;
            }
            MouseEventKind::Down(MouseButton::Left) => {}
            _ => return Action::None,
        }

        let Some(pos) = self.list_position(cols, rows, m.column as usize, m.row as usize) else {
            return Action::None;
        };
        if pos == self.sel {
            return self.activate();
        }
        self.sel = pos;
        self.refresh_detail();
        Action::Redraw
    }

    /// The filtered-list position drawn under a screen cell, if that cell is
    /// inside the clickable part of the version list.
    fn list_position(&self, cols: usize, rows: usize, col: usize, row: usize) -> Option<usize> {
        list_cell(cols, rows, col, row, self.scroll, self.filtered.len())
    }

    /// The Enter semantics: play when installed, install otherwise.
    fn activate(&mut self) -> Action {
        if let Some(item) = self.selected() {
            if item.installed {
                Action::Play(item.name.clone())
            } else {
                Action::Install(item.name.clone())
            }
        } else {
            self.status = "no versions match the filter".into();
            Action::Redraw
        }
    }

    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Action {
        match self.mode {
            Mode::Browse => self.on_key_browse(code, mods),
            Mode::Filter => {
                match code {
                    KeyCode::Esc | KeyCode::Enter => self.mode = Mode::Browse,
                    KeyCode::Backspace => {
                        self.filter.pop();
                    }
                    KeyCode::Char(c) => self.filter.push(c),
                    KeyCode::Up => self.move_sel(-1),
                    KeyCode::Down => self.move_sel(1),
                    _ => {}
                }
                self.refilter();
                self.refresh_detail();
                Action::Redraw
            }
            Mode::EditUsername | Mode::EditXmx | Mode::EditFps => {
                match code {
                    KeyCode::Esc => self.mode = Mode::Browse,
                    KeyCode::Enter => self.commit_edit(),
                    KeyCode::Backspace => {
                        self.edit_buf.pop();
                    }
                    KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => {
                        self.edit_buf.push(c)
                    }
                    _ => {}
                }
                Action::Redraw
            }
        }
    }

    fn on_key_browse(&mut self, code: KeyCode, mods: KeyModifiers) -> Action {
        if mods.contains(KeyModifiers::CONTROL)
            && matches!(code, KeyCode::Char('q') | KeyCode::Char('c'))
        {
            return Action::Quit;
        }
        match code {
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Esc => Action::Quit,
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_sel(-1);
                self.refresh_detail();
                Action::Redraw
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_sel(1);
                self.refresh_detail();
                Action::Redraw
            }
            KeyCode::PageUp => {
                self.move_sel(-10);
                self.refresh_detail();
                Action::Redraw
            }
            KeyCode::PageDown => {
                self.move_sel(10);
                self.refresh_detail();
                Action::Redraw
            }
            KeyCode::Home => {
                self.sel = 0;
                self.scroll_into_view(visible_rows());
                self.refresh_detail();
                Action::Redraw
            }
            KeyCode::End => {
                self.sel = self.filtered.len().saturating_sub(1);
                self.scroll_into_view(visible_rows());
                self.refresh_detail();
                Action::Redraw
            }
            KeyCode::Char('/') => {
                self.mode = Mode::Filter;
                Action::Redraw
            }
            KeyCode::Char('r') => {
                self.status = "reloading manifest…".into();
                self.reload();
                if self.status.is_empty() {
                    self.status = format!("{} versions", self.items.len());
                }
                self.refresh_detail();
                Action::Redraw
            }
            KeyCode::Char('n') => {
                self.mode = Mode::EditUsername;
                self.edit_buf = self.config.username.clone();
                Action::Redraw
            }
            KeyCode::Char('m') => {
                self.mode = Mode::EditXmx;
                self.edit_buf = self.config.xmx_mb.to_string();
                Action::Redraw
            }
            KeyCode::Char('p') => {
                self.mode = Mode::EditFps;
                self.edit_buf = self.config.fps.to_string();
                Action::Redraw
            }
            KeyCode::Char('c') => {
                self.config.cycle_resolution();
                let (cols, rows) = session::terminal_cells();
                let (w, h) = self.config.game_size(cols, rows);
                self.status = format!("resolution {} (game {w}x{h} px)", self.config.resolution);
                Action::Redraw
            }
            KeyCode::Char('g') => {
                self.config.low_end = !self.config.low_end;
                self.config.save();
                self.status = if self.config.low_end {
                    "graphics: fast (Minecraft tuned for speed on launch)".into()
                } else {
                    "graphics: vanilla (your own Minecraft settings)".into()
                };
                Action::Redraw
            }
            KeyCode::Char('u') => {
                self.config.mouse_recenter = !self.config.mouse_recenter;
                self.config.save();
                self.status = if self.config.mouse_recenter {
                    "edge mouse: endless — the camera keeps turning at the screen edge".into()
                } else {
                    "edge mouse: limited — turning stops at the screen edge".into()
                };
                Action::Redraw
            }
            KeyCode::Char('i') => {
                self.status = "select a version first".into();
                if let Some(item) = self.selected() {
                    if item.installed {
                        self.status = format!("{} already installed", item.name);
                    } else {
                        return Action::Install(item.name.clone());
                    }
                }
                Action::Redraw
            }
            KeyCode::Enter => self.activate(),
            _ => Action::None,
        }
    }

    fn move_sel(&mut self, delta: i32) {
        if self.filtered.is_empty() {
            return;
        }
        let len = self.filtered.len() as i32;
        let next = (self.sel as i32 + delta).clamp(0, len - 1);
        self.sel = next as usize;
        self.scroll_into_view(visible_rows());
    }

    fn commit_edit(&mut self) {
        let buf = self.edit_buf.trim().to_string();
        match self.mode {
            Mode::EditUsername => {
                if !buf.is_empty() {
                    self.config.username = buf;
                }
            }
            Mode::EditXmx => {
                if let Ok(v) = buf.parse::<u32>() {
                    self.config.xmx_mb = v.clamp(256, 65536);
                }
            }
            Mode::EditFps => {
                if let Ok(v) = buf.parse::<u32>() {
                    self.config.fps = v.clamp(1, 120);
                }
            }
            _ => {}
        }
        self.mode = Mode::Browse;
        self.config.save();
    }
}

// -------------------------------------------------------------- box helpers

fn border_edge() -> Span {
    Span::plain("║").dim()
}

fn box_top(title: &str, width: usize) -> Vec<Span> {
    // ┌ Title ────────────┐
    let mut spans = vec![Span::plain("┌").dim(), Span::plain(title).fg(Color::Magenta).bold()];
    let used = 1 + title.chars().count();
    let fill = width.saturating_sub(used + 1);
    spans.push(Span::plain("─".repeat(fill)).dim());
    spans.push(Span::plain("┐").dim());
    spans
}

fn box_bottom(width: usize) -> Vec<Span> {
    vec![Span::plain(format!("└{}┘", "─".repeat(width.saturating_sub(2)))).dim()]
}

/// Add the left `│` and right `│` of an inner box around a content row.
fn prepend_bar(spans: Vec<Span>) -> Vec<Span> {
    let mut out = vec![Span::plain("│").dim()];
    out.extend(spans);
    out.push(Span::plain("│").dim());
    out
}

fn empty_row(inner: usize, _style: Option<Color>) -> Vec<Span> {
    vec![Span::plain("│").dim(), Span::plain(" ".repeat(inner)), Span::plain("│").dim()]
}

fn kv(key: &str, value: Vec<Span>) -> Vec<Span> {
    let mut spans = vec![Span::plain(format!(" {key:<12}")).dim()];
    spans.extend(value);
    spans
}

enum Action {
    None,
    Redraw,
    Quit,
    Play(String),
    Install(String),
}

/// Fetch the remote manifest, using the on-disk cache when offline.
fn load_manifest(config: &Config) -> Result<Vec<VersionEntry>> {
    match mc::fetch_manifest() {
        Ok(m) => {
            let _ = std::fs::write(
                config.manifest_cache_path(),
                serde_json::to_string(&m)?,
            );
            Ok(m.versions)
        }
        Err(e) => {
            let text = std::fs::read_to_string(config.manifest_cache_path())
                .with_context(|| format!("fetch: {e}; no cache"))?;
            let m: mc::Manifest = serde_json::from_str(&text)?;
            Ok(m.versions)
        }
    }
}

pub fn run(config: Config) -> Result<()> {
    display::require_fullscreen()?;

    let mut menu = Menu::new(config);
    if menu.items.is_empty() {
        menu.status = "no versions found — check network".into();
    } else if menu.status.is_empty() {
        menu.status = format!("{} versions", menu.items.len());
    }
    menu.refresh_detail();

    tui_enter()?;
    let result = event_loop(&mut menu);
    tui_leave()?;
    result
}

fn event_loop(menu: &mut Menu) -> Result<()> {
    menu.draw()?;
    loop {
        if crate::signals::shutdown_requested() {
            return Ok(());
        }
        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let ev = match event::read() {
            Ok(ev) => ev,
            // the terminal went away; leave the TUI quietly
            Err(_) => return Ok(()),
        };
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => {
                let action = menu.on_key(k.code, k.modifiers);
                if menu.apply_action(action)? {
                    return Ok(());
                }
            }
            Event::Mouse(m) => {
                let action = menu.on_mouse(m);
                if menu.apply_action(action)? {
                    return Ok(());
                }
            }
            Event::Resize(_, _) => menu.draw()?,
            _ => {}
        }
    }
}

impl Menu {
    /// Carry out an action from the keyboard or the mouse. Returns `true`
    /// when the launcher should quit.
    fn apply_action(&mut self, action: Action) -> Result<bool> {
        match action {
            Action::None => {}
            Action::Redraw => self.draw()?,
            Action::Quit => return Ok(true),
            Action::Play(version) => {
                self.config.last_version = Some(version.clone());
                self.config.save();
                let (cols, rows) = session::terminal_cells();
                let (game_w, game_h) = self.config.game_size(cols, rows);
                let params = session::SessionParams {
                    version,
                    game_w,
                    game_h,
                    fps: self.config.fps.clamp(1, 120),
                    xmx_mb: self.config.xmx_mb,
                    username: self.config.username.clone(),
                };
                // give the terminal to the session, then take it back
                tui_leave()?;
                let res = session::run(&self.config, &params);
                let reenter = tui_enter();
                // A filter left over from before the session would keep the
                // list showing only the version just played; return to the
                // full list with that version selected.
                self.filter.clear();
                self.refilter();
                self.scroll = self.sel.saturating_sub(visible_rows() / 2);
                self.refresh_detail();
                match res {
                    Ok(s) => {
                        self.status = format!(
                            "session ended — {:.0} fps shown, {:.0} fps from Minecraft",
                            s.fps(),
                            s.game_fps()
                        )
                    }
                    Err(e) => self.status = format!("error: {e:#}"),
                }
                reenter?;
                self.draw()?;
            }
            Action::Install(version) => {
                self.status = format!("installing {version}…");
                self.draw()?;
                let gdir = self.config.game_dir.clone();
                let v = version.clone();
                let res = mc::install(&gdir, &version, |line| {
                    // progress: overwrite the status line
                    let (w, h) = crossterm::terminal::size().unwrap_or((80, 24));
                    let mut out = io::stdout();
                    let _ = queue!(
                        out,
                        cursor::MoveTo(3, h.saturating_sub(2)),
                        Clear(ClearType::CurrentLine),
                        crossterm::style::Print(fit(line, w.saturating_sub(4) as usize)),
                    );
                    let _ = out.flush();
                });
                match res {
                    Ok(()) => {
                        self.status = format!("{v} installed ✓");
                        self.reload();
                    }
                    Err(e) => self.status = format!("error: install failed: {e:#}"),
                }
                self.refresh_detail();
                self.draw()?;
            }
        }
        Ok(false)
    }
}

fn fit(s: &str, w: usize) -> String {
    s.chars().take(w).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // 120x40 terminal: box_h = 34, so 32 version rows, rows 2..=33
    const COLS: usize = 120;
    const ROWS: usize = 40;

    #[test]
    fn the_first_version_row_maps_to_the_top_of_the_list() {
        assert_eq!(list_cell(COLS, ROWS, 10, 2, 0, 100), Some(0));
        assert_eq!(list_cell(COLS, ROWS, 10, 3, 0, 100), Some(1));
    }

    #[test]
    fn the_row_under_the_cursor_is_the_one_that_gets_selected() {
        // scrolled to 50: row 2 shows item 50, row 5 shows item 53
        assert_eq!(list_cell(COLS, ROWS, 10, 2, 50, 100), Some(50));
        assert_eq!(list_cell(COLS, ROWS, 10, 5, 50, 100), Some(53));
    }

    #[test]
    fn clicks_outside_the_version_rows_do_nothing() {
        // header and the panel border above the list
        assert_eq!(list_cell(COLS, ROWS, 10, 0, 0, 100), None);
        assert_eq!(list_cell(COLS, ROWS, 10, 1, 0, 100), None);
        // status/footer below the list: the last version row is 2 + 32 - 1
        assert_eq!(list_cell(COLS, ROWS, 10, 34, 0, 100), None);
    }

    #[test]
    fn clicks_outside_the_left_panel_do_nothing() {
        assert_eq!(list_cell(COLS, ROWS, 2, 5, 0, 100), None);
        // the detail panel starts after the list
        assert_eq!(list_cell(COLS, ROWS, 80, 5, 0, 100), None);
        // and the tiny-layout terminal has no list at all
        assert_eq!(list_cell(40, 10, 10, 2, 0, 100), None);
    }

    #[test]
    fn empty_rows_below_a_short_list_are_ignored() {
        // three items: rows 2, 3, 4 are real, row 5 is empty space
        assert_eq!(list_cell(COLS, ROWS, 10, 4, 0, 3), Some(2));
        assert_eq!(list_cell(COLS, ROWS, 10, 5, 0, 3), None);
    }

    #[test]
    fn a_scroll_past_the_end_is_clamped_like_the_drawer_does() {
        // the drawer clamps `scroll` to len - list_h, so the mapping follows
        assert_eq!(list_cell(COLS, ROWS, 10, 2, 999, 100), Some(68));
        assert_eq!(list_cell(COLS, ROWS, 10, 33, 999, 100), Some(99));
    }
}
