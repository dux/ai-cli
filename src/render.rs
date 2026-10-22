//! Paints the emulated child screen into rows 1..N of the real terminal and
//! the bar into the rows below it.
//!
//! The real terminal only ever receives what this module writes: absolute
//! cursor moves, cell text, SGR, erase-in-line - all aimed at rows we chose.
//! The child cannot reach the bar because none of its bytes get through, and
//! the bar never flickers because it is only written when its own content
//! changes.
//!
//! `front` is what the real terminal shows right now, row by row; a frame is
//! the difference between that and the emulator's grid. Lines the child
//! scrolls off the top go into the real terminal's scrollback the way they
//! would without us: painted at the top of the region, then scrolled out of it.

use std::fmt::Write as _;

use alacritty_terminal::Term;
use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::{Dimensions, Row};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::vte::ansi::{Color, NamedColor};

/// Most lines handed to the real scrollback in one frame. A child that dumps
/// more than this between two frames loses the oldest from scrollback only.
const PUSH_CAP: usize = 5000;

const IGNORED: Flags = Flags::WRAPLINE;

pub struct Renderer {
    pub rows: usize,
    pub cols: usize,
    pub child_rows: usize,
    front: Vec<Option<Vec<Cell>>>,
    bar: Vec<String>,
    bar_dirty: bool,
    hist_seen: usize,
    was_alt: bool,
    out: String,
}

impl Renderer {
    pub fn new(rows: usize, cols: usize, child_rows: usize) -> Renderer {
        Renderer {
            rows,
            cols,
            child_rows,
            front: vec![None; child_rows],
            bar: Vec::new(),
            bar_dirty: true,
            hist_seen: 0,
            was_alt: false,
            out: String::new(),
        }
    }

    /// The screen geometry changed, or something else may have scribbled on
    /// it: forget what is there and repaint everything next frame. History
    /// already in the real scrollback stays where it is.
    pub fn invalidate<T>(&mut self, term: &Term<T>, rows: usize, cols: usize, child_rows: usize) {
        self.rows = rows;
        self.cols = cols;
        self.child_rows = child_rows;
        self.front = vec![None; child_rows];
        self.bar_dirty = true;
        if !term.mode().contains(TermMode::ALT_SCREEN) {
            self.hist_seen = term.grid().history_size();
        }
    }

    /// Bytes that set the real terminal up for us: no autowrap (a cell in the
    /// last column must never scroll anything), and a scroll region that ends
    /// above the bar, used only to push history into scrollback.
    pub fn region(&self) -> String {
        format!("\x1b[?7l\x1b[1;{}r", self.child_rows)
    }

    pub fn set_bar(&mut self, lines: Vec<String>) {
        if lines != self.bar {
            self.bar = lines;
            self.bar_dirty = true;
        }
    }

    pub fn bar_rows(&self) -> usize {
        self.rows - self.child_rows
    }

    /// One frame: history first, then changed rows, the bar if it changed,
    /// and the cursor where the child left it. Wrapped in a synchronized
    /// update so the terminal shows it all at once.
    pub fn frame<T: EventListener>(&mut self, term: &mut Term<T>, history_reset: bool) -> &str {
        self.out.clear();
        self.out.push_str("\x1b[?2026h\x1b[?25l");

        if term.grid().display_offset() != 0 {
            term.scroll_display(alacritty_terminal::grid::Scroll::Bottom);
        }

        let alt = term.mode().contains(TermMode::ALT_SCREEN);
        if !alt {
            let h = term.grid().history_size();
            if history_reset {
                self.hist_seen = 0;
            }
            if self.was_alt {
                self.hist_seen = h;
            }
            if h > self.hist_seen {
                self.push_history(term, h - self.hist_seen);
            }
            self.hist_seen = h;
        }
        self.was_alt = alt;

        let grid = term.grid();
        let lines = grid.screen_lines().min(self.child_rows);
        for r in 0..lines {
            let row = &grid[Line(r as i32)];
            if !self.same(r, row) {
                self.draw_row(r, row);
            }
        }

        if self.bar_dirty {
            self.draw_bar();
        }

        let cursor = grid.cursor.point;
        if term.mode().contains(TermMode::SHOW_CURSOR) && (cursor.line.0 as usize) < self.child_rows {
            let col = cursor.column.0.min(self.cols.saturating_sub(1));
            let _ = write!(self.out, "\x1b[{};{}H\x1b[?25h", cursor.line.0 + 1, col + 1);
        }
        self.out.push_str("\x1b[?2026l");
        &self.out
    }

    /// Hands `k` new history lines to the real terminal's scrollback, oldest
    /// first: paint a chunk into the top rows, then scroll it out of the
    /// region from its bottom row. When the chunk is what is already on screen,
    /// the common case of the child printing a line and everything moving up,
    /// the paint is free and only the newlines go out.
    fn push_history<T>(&mut self, term: &Term<T>, k: usize) {
        let grid = term.grid();
        let k = k.min(grid.history_size()).min(PUSH_CAP);
        let n = self.child_rows;
        let mut next = k; // distance above the screen of the next line to push
        while next > 0 {
            let m = next.min(n);
            for j in 0..m {
                let line = Line(-((next - j) as i32));
                let row = &grid[line];
                if !self.same(j, row) {
                    self.draw_row(j, row);
                }
            }
            let _ = write!(self.out, "\x1b[{n};1H\x1b[0m");
            for _ in 0..m {
                self.out.push('\n');
            }
            self.front.rotate_left(m);
            let blank = vec![Cell::default(); self.cols];
            for slot in self.front.iter_mut().rev().take(m) {
                *slot = Some(blank.clone());
            }
            next -= m;
        }
    }

    fn same(&self, r: usize, row: &Row<Cell>) -> bool {
        let Some(Some(front)) = self.front.get(r) else { return false };
        front.len() == self.cols && (0..self.cols).all(|c| same_cell(&front[c], &row[Column(c)]))
    }

    fn draw_row(&mut self, r: usize, row: &Row<Cell>) {
        let cols = self.cols.min(row.len());
        let _ = write!(self.out, "\x1b[{};1H\x1b[0m", r + 1);

        let mut last = None;
        for c in (0..cols).rev() {
            if !blank(&row[Column(c)]) {
                last = Some(c);
                break;
            }
        }

        let mut pen = Pen::default();
        let mut link: Option<String> = None;
        if let Some(last) = last {
            for c in 0..=last {
                let cell = &row[Column(c)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                let want = Pen::of(cell);
                if want != pen {
                    want.sgr(&mut self.out);
                    pen = want;
                }
                let href = cell.hyperlink().map(|h| format!("id={};{}", h.id(), h.uri()));
                if href != link {
                    let _ = write!(self.out, "\x1b]8;{}\x1b\\", href.as_deref().unwrap_or(";"));
                    link = href;
                }
                if cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) || cell.c < ' ' {
                    self.out.push(' ');
                } else {
                    self.out.push(cell.c);
                    if let Some(zw) = cell.zerowidth() {
                        self.out.extend(zw.iter());
                    }
                }
            }
        }
        if link.is_some() {
            self.out.push_str("\x1b]8;;\x1b\\");
        }
        if last.map_or(0, |l| l + 1) < self.cols {
            if pen != Pen::default() {
                self.out.push_str("\x1b[0m");
            }
            self.out.push_str("\x1b[K");
        } else if pen != Pen::default() {
            self.out.push_str("\x1b[0m");
        }

        let mut cells: Vec<Cell> = (0..cols).map(|c| row[Column(c)].clone()).collect();
        cells.resize(self.cols, Cell::default());
        if let Some(slot) = self.front.get_mut(r) {
            *slot = Some(cells);
        }
    }

    fn draw_bar(&mut self) {
        for i in 0..self.bar_rows() {
            let line = self.bar.get(i).map(String::as_str).unwrap_or("");
            let _ = write!(self.out, "\x1b[{};1H\x1b[0m\x1b[2K{}\x1b[0m", self.child_rows + 1 + i, line);
        }
        self.bar_dirty = false;
    }
}

fn same_cell(a: &Cell, b: &Cell) -> bool {
    a.c == b.c && a.fg == b.fg && a.bg == b.bg && (a.flags - IGNORED) == (b.flags - IGNORED) && a.extra == b.extra
}

/// A cell that erase-in-line would leave behind anyway.
fn blank(cell: &Cell) -> bool {
    (cell.c == ' ' || cell.c == '\0')
        && cell.bg == Color::Named(NamedColor::Background)
        && (cell.flags - IGNORED - Flags::BOLD - Flags::DIM - Flags::ITALIC).is_empty()
        && cell.extra.is_none()
}

/// The SGR state a cell wants.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Pen {
    fg: Color,
    bg: Color,
    ul: Option<Color>,
    flags: Flags,
}

impl Default for Pen {
    fn default() -> Pen {
        Pen { fg: Color::Named(NamedColor::Foreground), bg: Color::Named(NamedColor::Background), ul: None, flags: Flags::empty() }
    }
}

const STYLE: Flags = Flags::BOLD
    .union(Flags::DIM)
    .union(Flags::ITALIC)
    .union(Flags::ALL_UNDERLINES)
    .union(Flags::INVERSE)
    .union(Flags::HIDDEN)
    .union(Flags::STRIKEOUT);

impl Pen {
    fn of(cell: &Cell) -> Pen {
        Pen { fg: cell.fg, bg: cell.bg, ul: cell.underline_color(), flags: cell.flags & STYLE }
    }

    fn sgr(&self, out: &mut String) {
        out.push_str("\x1b[0");
        let f = self.flags;
        if f.contains(Flags::BOLD) {
            out.push_str(";1");
        }
        if f.contains(Flags::DIM) {
            out.push_str(";2");
        }
        if f.contains(Flags::ITALIC) {
            out.push_str(";3");
        }
        if f.contains(Flags::UNDERLINE) {
            out.push_str(";4");
        } else if f.contains(Flags::DOUBLE_UNDERLINE) {
            out.push_str(";4:2");
        } else if f.contains(Flags::UNDERCURL) {
            out.push_str(";4:3");
        } else if f.contains(Flags::DOTTED_UNDERLINE) {
            out.push_str(";4:4");
        } else if f.contains(Flags::DASHED_UNDERLINE) {
            out.push_str(";4:5");
        }
        if f.contains(Flags::INVERSE) {
            out.push_str(";7");
        }
        if f.contains(Flags::HIDDEN) {
            out.push_str(";8");
        }
        if f.contains(Flags::STRIKEOUT) {
            out.push_str(";9");
        }
        color(out, self.fg, 30, 90, 38);
        color(out, self.bg, 40, 100, 48);
        if let Some(ul) = self.ul {
            match ul {
                Color::Spec(c) => {
                    let _ = write!(out, ";58:2::{}:{}:{}", c.r, c.g, c.b);
                }
                Color::Indexed(i) => {
                    let _ = write!(out, ";58:5:{i}");
                }
                Color::Named(n) => {
                    if let Some(i) = palette_index(n) {
                        let _ = write!(out, ";58:5:{i}");
                    }
                }
            }
        }
        out.push('m');
    }
}

/// 0-15 for the sixteen palette colours, their dim variants included.
fn palette_index(n: NamedColor) -> Option<u8> {
    let v = n as usize;
    if v < 16 {
        return Some(v as u8);
    }
    let dim = NamedColor::DimBlack as usize;
    if (dim..dim + 8).contains(&v) {
        return Some((v - dim) as u8);
    }
    None
}

fn color(out: &mut String, c: Color, base: u8, bright: u8, ext: u8) {
    match c {
        Color::Named(n) => match palette_index(n) {
            Some(i) if i < 8 => {
                let _ = write!(out, ";{}", base + i);
            }
            Some(i) => {
                let _ = write!(out, ";{}", bright + i - 8);
            }
            None => {} // the default colour: already set by the leading 0
        },
        Color::Indexed(i) => {
            let _ = write!(out, ";{ext};5;{i}");
        }
        Color::Spec(rgb) => {
            let _ = write!(out, ";{ext};2;{};{};{}", rgb.r, rgb.g, rgb.b);
        }
    }
}
