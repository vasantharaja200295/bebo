#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! bebo: a GPU terminal emulator. See docs/PLAN.md.

mod pty;
mod render;

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, WindowSize};
use alacritty_terminal::event_loop::{EventLoopSender, Msg};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Boundary, Column, Direction, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::search::RegexSearch;
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, Processor, StdSyncHandler};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{CursorIcon, Window, WindowId};

use pty::{Mark, Proxy, SharedTerm, TermSize, UserEvent};
use render::{Frame, Gpu, Quad, Row, Span};

const BG: [u8; 3] = [0x14, 0x15, 0x1b];
const FG: [u8; 3] = [0xd6, 0xd9, 0xe0];
const CURSOR: [u8; 3] = [0xe6, 0xc0, 0x7b];
const SELECTION: [u8; 3] = [0x33, 0x4a, 0x7a];
const BAR: [u8; 3] = [0x0d, 0x0e, 0x12];
const DIVIDER: [u8; 3] = [0x3a, 0x3d, 0x4a];
const SEPARATOR: [u8; 3] = [0x26, 0x28, 0x33];
const ERROR: [u8; 3] = [0xe0, 0x6c, 0x75];
const PAD: f32 = 6.0;
const FONT_PX: f32 = 15.0;

#[derive(Clone, Copy)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl Rect {
    fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

struct Pane {
    term: SharedTerm,
    tx: EventLoopSender,
    /// Changes at each restart. Exit events of an old shell have an old value.
    generation: u32,
    cols: usize,
    rows: usize,
    /// Cell where the command text starts: (line + history size, column).
    input_start: Option<(i32, usize)>,
    /// The shell printed mark B. Read the cursor position at the next wake-up.
    await_input: bool,
    cwd: Option<PathBuf>,
    title: String,
    /// One block for each prompt, in line order.
    blocks: Vec<Block>,
}

/// A prompt, its command, and the output of the command.
struct Block {
    /// Line of the prompt: grid line + history size.
    line: i32,
    exec_at: Option<Instant>,
    exit: Option<i32>,
    duration: Option<Duration>,
}

enum Node {
    Leaf(usize),
    /// `side_by_side`: `a` is left and `b` is right. If not, `a` is above `b`.
    Split { side_by_side: bool, ratio: f32, a: Box<Node>, b: Box<Node> },
}

impl Node {
    fn leaves(&self, out: &mut Vec<usize>) {
        match self {
            Node::Leaf(id) => out.push(*id),
            Node::Split { a, b, .. } => {
                a.leaves(out);
                b.leaves(out);
            },
        }
    }

    /// Replaces leaf `id` with a split of `id` and `new`.
    fn split(&mut self, id: usize, new: usize, side_by_side: bool) {
        match self {
            Node::Leaf(leaf) if *leaf == id => {
                *self = Node::Split {
                    side_by_side,
                    ratio: 0.5,
                    a: Box::new(Node::Leaf(id)),
                    b: Box::new(Node::Leaf(new)),
                };
            },
            Node::Leaf(_) => {},
            Node::Split { a, b, .. } => {
                a.split(id, new, side_by_side);
                b.split(id, new, side_by_side);
            },
        }
    }

    /// Removes leaf `id`. Returns `None` if the tree is then empty.
    fn remove(self, id: usize) -> Option<Node> {
        match self {
            Node::Leaf(leaf) if leaf == id => None,
            Node::Leaf(_) => Some(self),
            Node::Split { side_by_side, ratio, a, b } => match (a.remove(id), b.remove(id)) {
                (Some(a), Some(b)) => {
                    Some(Node::Split { side_by_side, ratio, a: Box::new(a), b: Box::new(b) })
                },
                (Some(n), None) | (None, Some(n)) => Some(n),
                (None, None) => None,
            },
        }
    }

    fn ratio_mut(&mut self, path: &[bool]) -> Option<&mut f32> {
        match self {
            Node::Leaf(_) => None,
            Node::Split { ratio, a, b, .. } => match path.split_first() {
                None => Some(ratio),
                Some((false, rest)) => a.ratio_mut(rest),
                Some((true, rest)) => b.ratio_mut(rest),
            },
        }
    }
}

struct Tab {
    root: Node,
    focus: usize,
}

struct Divider {
    rect: Rect,
    parent: Rect,
    side_by_side: bool,
    path: Vec<bool>,
}

fn layout(node: &Node, r: Rect, path: &mut Vec<bool>, panes: &mut Vec<(usize, Rect)>, dividers: &mut Vec<Divider>) {
    match node {
        Node::Leaf(id) => panes.push((*id, r)),
        Node::Split { side_by_side, ratio, a, b } => {
            let (ra, rb, rect) = if *side_by_side {
                let w = (r.w * ratio).round();
                (Rect { w, ..r }, Rect { x: r.x + w + 1.0, w: r.w - w - 1.0, ..r }, Rect { x: r.x + w, w: 1.0, ..r })
            } else {
                let h = (r.h * ratio).round();
                (Rect { h, ..r }, Rect { y: r.y + h + 1.0, h: r.h - h - 1.0, ..r }, Rect { y: r.y + h, h: 1.0, ..r })
            };
            dividers.push(Divider { rect, parent: r, side_by_side: *side_by_side, path: path.clone() });
            path.push(false);
            layout(a, ra, path, panes, dividers);
            path.pop();
            path.push(true);
            layout(b, rb, path, panes, dividers);
            path.pop();
        },
    }
}

/// The command line that the user edits now, in grid cells.
struct Region {
    start: Point,
    cols: usize,
    cursor: i64,
    end: i64,
}

impl Region {
    fn new(pane: &Pane, term: &Term<Proxy>) -> Option<Region> {
        let (abs_line, col) = pane.input_start?;
        if term.mode().contains(TermMode::ALT_SCREEN) {
            return None;
        }
        let grid = term.grid();
        let cols = grid.columns();
        // ponytail: the line number is wrong after a clear-screen or when the scrollback is full.
        // Then the region check fails and bebo works as a usual terminal. Fix: track the mark in the grid.
        let start = Point::new(Line(abs_line - grid.history_size() as i32), Column(col));
        let cursor_point = grid.cursor.point;
        if cursor_point < start || start.line.0 < -(grid.history_size() as i32) {
            return None;
        }
        let mut region = Region { start, cols, cursor: 0, end: 0 };
        region.cursor = region.idx(cursor_point);
        region.end = region.cursor;
        // The command text ends at the last cell that is not blank.
        'find: for line in (cursor_point.line.0..grid.screen_lines() as i32).rev() {
            for col in (0..cols).rev() {
                if grid[Line(line)][Column(col)].c != ' ' {
                    region.end = region.end.max(region.idx(Point::new(Line(line), Column(col))) + 1);
                    break 'find;
                }
            }
        }
        Some(region)
    }

    fn idx(&self, p: Point) -> i64 {
        (p.line.0 - self.start.line.0) as i64 * self.cols as i64 + p.column.0 as i64 - self.start.column.0 as i64
    }

    fn point(&self, idx: i64) -> Point {
        let cell = self.start.column.0 as i64 + idx;
        Point::new(Line(self.start.line.0 + (cell / self.cols as i64) as i32), Column((cell % self.cols as i64) as usize))
    }

    /// Number of characters in the cells `a..b`. A double-width character uses two cells.
    fn chars(&self, term: &Term<Proxy>, a: i64, b: i64) -> usize {
        // ponytail: a continuation prompt (">> ") of a multi-line command counts as characters.
        (a..b)
            .filter(|&i| {
                let p = self.point(i);
                !term.grid()[p.line][p.column].flags.contains(Flags::WIDE_CHAR_SPACER)
            })
            .count()
    }

    /// Keys that move the shell cursor from cell `from` to cell `to`.
    fn arrows(&self, term: &Term<Proxy>, from: i64, to: i64, out: &mut Vec<u8>) {
        let app = term.mode().contains(TermMode::APP_CURSOR);
        let (key, n): (&[u8], usize) = if to >= from {
            (if app { b"\x1bOC" } else { b"\x1b[C" }, self.chars(term, from, to))
        } else {
            (if app { b"\x1bOD" } else { b"\x1b[D" }, self.chars(term, to, from))
        };
        for _ in 0..n {
            out.extend_from_slice(key);
        }
    }

    /// Keys that delete the selection, if the selection is in the command line.
    fn delete_selection(&self, term: &Term<Proxy>) -> Option<Vec<u8>> {
        let range = term.selection.as_ref()?.to_range(term)?;
        let a = self.idx(range.start).max(0);
        let b = (self.idx(range.end) + 1).min(self.end);
        if self.idx(range.start) < 0 || a >= b {
            return None;
        }
        let mut out = Vec::new();
        self.arrows(term, self.cursor, b, &mut out);
        out.extend(std::iter::repeat_n(0x7f, self.chars(term, a, b)));
        Some(out)
    }
}

struct Press {
    pane: usize,
    point: Point,
    side: Side,
    clicks: u8,
    moved: bool,
    /// The program in the terminal gets the mouse events.
    reported: bool,
}

struct App {
    proxy: EventLoopProxy<UserEvent>,
    start: Instant,
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    panes: HashMap<usize, Pane>,
    tabs: Vec<Tab>,
    active: usize,
    next_id: usize,
    pane_rects: Vec<(usize, Rect)>,
    dividers: Vec<Divider>,
    mods: ModifiersState,
    mouse: (f32, f32),
    press: Option<Press>,
    drag_divider: Option<usize>,
    last_click: (Instant, Point, u8),
    search: Option<String>,
    clipboard: Option<arboard::Clipboard>,
    gpu_rx: std::sync::mpsc::Receiver<render::Device>,
}

impl App {
    fn focus(&self) -> usize {
        self.tabs[self.active].focus
    }

    fn redraw(&self) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn send(&self, id: usize, bytes: Vec<u8>) {
        if let Some(pane) = self.panes.get(&id) {
            let _ = pane.tx.send(Msg::Input(Cow::Owned(bytes)));
        }
    }

    fn window_size(pane_cols: usize, pane_rows: usize, gpu: Option<&Gpu>) -> WindowSize {
        WindowSize {
            num_cols: pane_cols as u16,
            num_lines: pane_rows as u16,
            cell_width: gpu.map_or(8, |g| g.cell_w as u16),
            cell_height: gpu.map_or(18, |g| g.cell_h as u16),
        }
    }

    fn new_pane(&mut self, cwd: Option<PathBuf>) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        let (cols, rows) = (100, 30);
        let proxy = Proxy { id, generation: 0, proxy: self.proxy.clone() };
        let term = pty::new_term(proxy.clone(), cols, rows);
        let size = Self::window_size(cols, rows, self.gpu.as_ref());
        let tx = pty::spawn_shell(term.clone(), proxy, size, cwd.clone()).expect("cannot start the shell");
        let pane = Pane {
            term,
            tx,
            generation: 0,
            cols,
            rows,
            input_start: None,
            await_input: false,
            cwd,
            title: "bebo".into(),
            blocks: Vec::new(),
        };
        self.panes.insert(id, pane);
        id
    }

    fn focus_cwd(&self) -> Option<PathBuf> {
        self.panes.get(&self.focus()).and_then(|p| p.cwd.clone())
    }

    fn new_tab(&mut self) {
        let cwd = if self.tabs.is_empty() { None } else { self.focus_cwd() };
        let id = self.new_pane(cwd);
        self.tabs.push(Tab { root: Node::Leaf(id), focus: id });
        self.active = self.tabs.len() - 1;
        self.relayout();
    }

    fn split(&mut self, side_by_side: bool) {
        let focus = self.tabs[self.active].focus;
        let id = self.new_pane(self.focus_cwd());
        let tab = &mut self.tabs[self.active];
        tab.root.split(focus, id, side_by_side);
        tab.focus = id;
        self.relayout();
    }

    fn close_pane(&mut self, id: usize, event_loop: &ActiveEventLoop) {
        let Some(pane) = self.panes.remove(&id) else { return };
        let _ = pane.tx.send(Msg::Shutdown);
        let Some(index) = self.tabs.iter().position(|t| {
            let mut leaves = Vec::new();
            t.root.leaves(&mut leaves);
            leaves.contains(&id)
        }) else {
            return;
        };
        let root = std::mem::replace(&mut self.tabs[index].root, Node::Leaf(usize::MAX));
        match root.remove(id) {
            Some(root) => {
                let mut leaves = Vec::new();
                root.leaves(&mut leaves);
                let tab = &mut self.tabs[index];
                tab.root = root;
                if tab.focus == id {
                    tab.focus = leaves[0];
                }
            },
            None => {
                self.tabs.remove(index);
                if self.tabs.is_empty() {
                    event_loop.exit();
                    return;
                }
                if self.active >= index && self.active > 0 {
                    self.active -= 1;
                }
            },
        }
        self.relayout();
    }

    /// Stops the shell of the pane and starts a new shell with a new environment.
    fn restart(&mut self, id: usize) {
        let gpu = self.gpu.as_ref();
        let Some(pane) = self.panes.get_mut(&id) else { return };
        let _ = pane.tx.send(Msg::Shutdown);
        pane.generation += 1;
        pane.input_start = None;
        pane.await_input = false;
        {
            let mut term = pane.term.lock();
            let mut parser: Processor<StdSyncHandler> = Processor::new();
            parser.advance(&mut *term, b"\x1b[?1049l\x1b[0m\x1b[?25h\r\n\x1b[2m[restarted]\x1b[0m\r\n");
        }
        let proxy = Proxy { id, generation: pane.generation, proxy: self.proxy.clone() };
        let size = Self::window_size(pane.cols, pane.rows, gpu);
        match pty::spawn_shell(pane.term.clone(), proxy, size, pane.cwd.clone()) {
            Ok(tx) => pane.tx = tx,
            Err(err) => {
                let mut term = pane.term.lock();
                let mut parser: Processor<StdSyncHandler> = Processor::new();
                parser.advance(&mut *term, format!("cannot start the shell: {err}\r\n").as_bytes());
            },
        }
        self.redraw();
    }

    /// Makes the renderer when the window and the GPU device are ready, then draws the first frame.
    fn start_gpu(&mut self) {
        let Some(window) = self.window.clone() else { return };
        if self.gpu.is_some() {
            return;
        }
        let Ok(parts) = self.gpu_rx.try_recv() else { return };
        let font_px = FONT_PX * window.scale_factor() as f32;
        self.gpu = Some(Gpu::new(window, parts, font_px, self.start));
        render::tick(self.start, "gpu ready");
        self.relayout();
        self.draw();

        // Test aid: BEBO_TEST=tde runs these commands at start (t: tab, d: split right, e: split down, f: search).
        for command in std::env::var("BEBO_TEST").unwrap_or_default().chars() {
            match command {
                't' => self.new_tab(),
                'd' => self.split(true),
                'e' => self.split(false),
                'f' => {
                    self.search = Some("bebo".into());
                    self.relayout();
                },
                _ => {},
            }
        }

        if std::env::var_os("BEBO_TIMING").is_some() {
            let gpu = self.gpu.as_ref().unwrap();
            render::tick(self.start, &format!("first frame, adapter: {}", gpu.adapter_name));
            let _ = std::fs::write(std::env::temp_dir().join("bebo-timing.txt"), &*render::TIMING.lock().unwrap());
        }
    }

    fn bar_heights(&self) -> (f32, f32) {
        let cell_h = self.gpu.as_ref().map_or(18.0, |g| g.cell_h);
        let tab_bar = if self.tabs.len() > 1 { cell_h + 10.0 } else { 0.0 };
        let search_bar = if self.search.is_some() { cell_h + 8.0 } else { 0.0 };
        (tab_bar, search_bar)
    }

    /// Calculates the pane rectangles and sends a new size to each shell that needs it.
    fn relayout(&mut self) {
        let Some(gpu) = self.gpu.as_ref() else { return };
        let (width, height) = gpu.size();
        let (tab_bar, search_bar) = self.bar_heights();
        let area = Rect { x: 0.0, y: tab_bar, w: width, h: (height - tab_bar - search_bar).max(1.0) };
        self.pane_rects.clear();
        self.dividers.clear();
        layout(&self.tabs[self.active].root, area, &mut Vec::new(), &mut self.pane_rects, &mut self.dividers);
        for (id, rect) in &self.pane_rects {
            let Some(pane) = self.panes.get_mut(id) else { continue };
            let cols = (((rect.w - 2.0 * PAD) / gpu.cell_w) as usize).max(2);
            let rows = (((rect.h - 2.0 * PAD) / gpu.cell_h) as usize).max(1);
            if (cols, rows) != (pane.cols, pane.rows) {
                pane.cols = cols;
                pane.rows = rows;
                // A new width moves the lines, so the block lines are not correct after a resize.
                pane.blocks.clear();
                pane.term.lock().resize(TermSize { cols, rows });
                let _ = pane.tx.send(Msg::Resize(Self::window_size(cols, rows, Some(gpu))));
            }
        }
        if let (Some(window), Some(pane)) = (&self.window, self.panes.get(&self.focus())) {
            window.set_title(&pane.title);
        }
        self.redraw();
    }

    fn draw(&mut self) {
        let Some(gpu) = self.gpu.as_mut() else { return };
        let (width, height) = gpu.size();
        let (cw, ch) = (gpu.cell_w, gpu.cell_h);
        let mut frame = Frame::default();
        let text = |x: f32, y: f32, clip: Rect, s: &str, color: [u8; 3]| Row {
            x,
            y,
            clip: [clip.x, clip.y, clip.x + clip.w, clip.y + clip.h],
            spans: vec![Span { text: s.to_string(), color, bold: false, italic: false }],
        };

        // Tab bar. It shows only with two or more tabs.
        if self.tabs.len() > 1 {
            let bar_h = ch + 10.0;
            frame.quads.push(Quad { x: 0.0, y: 0.0, w: width, h: bar_h, color: rgba(BAR) });
            let tab_w = (width / self.tabs.len() as f32).min(260.0);
            for (i, tab) in self.tabs.iter().enumerate() {
                let rect = Rect { x: i as f32 * tab_w, y: 0.0, w: tab_w - 1.0, h: bar_h };
                let active = i == self.active;
                if active {
                    frame.quads.push(Quad { x: rect.x, y: 0.0, w: rect.w, h: bar_h, color: rgba(BG) });
                    frame.quads.push(Quad { x: rect.x, y: 0.0, w: rect.w, h: 2.0, color: rgba(CURSOR) });
                }
                let title = self.panes.get(&tab.focus).map_or("", |p| p.title.as_str());
                let max = ((tab_w - 16.0) / cw) as usize;
                let title: String = title.chars().rev().take(max).collect::<Vec<_>>().into_iter().rev().collect();
                let color = if active { FG } else { dim(FG) };
                frame.rows.push(text(rect.x + 8.0, 5.0, rect, &title, color));
            }
        }

        let focus = self.tabs[self.active].focus;
        for (id, rect) in &self.pane_rects {
            if let Some(pane) = self.panes.get(id) {
                draw_pane(&mut frame, pane, *rect, *id == focus, cw, ch);
            }
        }
        for d in &self.dividers {
            frame.overlay.push(Quad { x: d.rect.x, y: d.rect.y, w: d.rect.w, h: d.rect.h, color: rgba(DIVIDER) });
        }

        if let Some(pattern) = &self.search {
            let bar = Rect { x: 0.0, y: height - ch - 8.0, w: width, h: ch + 8.0 };
            frame.quads.push(Quad { x: bar.x, y: bar.y, w: bar.w, h: bar.h, color: rgba(BAR) });
            let label = format!("search (regex): {pattern}_   Enter: older   Shift+Enter: newer   Esc: close");
            frame.rows.push(text(8.0, bar.y + 4.0, bar, &label, FG));
        }

        gpu.render(&frame, BG);
    }

    fn pane_at(&self, x: f32, y: f32) -> Option<(usize, Rect)> {
        self.pane_rects.iter().copied().find(|(_, r)| r.contains(x, y))
    }

    fn cell_at(&self, id: usize, rect: Rect, x: f32, y: f32) -> Option<(Point, Side)> {
        let gpu = self.gpu.as_ref()?;
        let pane = self.panes.get(&id)?;
        let col = ((x - rect.x - PAD) / gpu.cell_w).max(0.0);
        let side = if col.fract() < 0.5 { Side::Left } else { Side::Right };
        let col = (col as usize).min(pane.cols - 1);
        let row = (((y - rect.y - PAD) / gpu.cell_h).max(0.0) as usize).min(pane.rows - 1);
        let offset = pane.term.lock().grid().display_offset() as i32;
        Some((Point::new(Line(row as i32 - offset), Column(col)), side))
    }

    fn mouse_report(&self, id: usize, button: u8, point: Point, pressed: bool) {
        let Some(pane) = self.panes.get(&id) else { return };
        let (col, row) = (point.column.0 + 1, point.line.0 + 1);
        let sgr = pane.term.lock().mode().contains(TermMode::SGR_MOUSE);
        let bytes = if sgr {
            format!("\x1b[<{button};{col};{row}{}", if pressed { 'M' } else { 'm' }).into_bytes()
        } else if col < 223 && row < 223 {
            let button = if pressed { button } else { 3 };
            vec![0x1b, b'[', b'M', 32 + button, 32 + col as u8, 32 + row as u8]
        } else {
            return;
        };
        self.send(id, bytes);
    }

    fn mouse_press(&mut self) {
        let (x, y) = self.mouse;
        let (tab_bar, _) = self.bar_heights();
        if y < tab_bar {
            let tab_w = (self.gpu.as_ref().map_or(1.0, |g| g.size().0) / self.tabs.len() as f32).min(260.0);
            let index = (x / tab_w) as usize;
            if index < self.tabs.len() {
                self.active = index;
                self.relayout();
            }
            return;
        }
        if let Some(i) = self.divider_at(x, y) {
            self.drag_divider = Some(i);
            return;
        }
        let Some((id, rect)) = self.pane_at(x, y) else { return };
        self.tabs[self.active].focus = id;
        let Some((point, side)) = self.cell_at(id, rect, x, y) else { return };
        if self.mods.control_key() && self.open_link(id, point) {
            return;
        }

        let now = Instant::now();
        let (last_time, last_point, last_count) = self.last_click;
        let clicks = if now - last_time < Duration::from_millis(400) && last_point == point { last_count % 3 + 1 } else { 1 };
        self.last_click = (now, point, clicks);

        let pane = &self.panes[&id];
        let mut term = pane.term.lock();
        let reported = term.mode().intersects(TermMode::MOUSE_MODE) && !self.mods.shift_key();
        term.selection = match clicks {
            _ if reported => None,
            2 => Some(Selection::new(SelectionType::Semantic, point, side)),
            3 => Some(Selection::new(SelectionType::Lines, point, side)),
            _ => None,
        };
        drop(term);
        if reported {
            self.mouse_report(id, 0, point, true);
        }
        self.press = Some(Press { pane: id, point, side, clicks, moved: false, reported });
        self.relayout();
    }

    fn mouse_move(&mut self) {
        let (x, y) = self.mouse;
        if let Some(i) = self.drag_divider {
            let d = &self.dividers[i];
            let ratio = if d.side_by_side { (x - d.parent.x) / d.parent.w } else { (y - d.parent.y) / d.parent.h };
            let path = d.path.clone();
            if let Some(r) = self.tabs[self.active].root.ratio_mut(&path) {
                *r = ratio.clamp(0.1, 0.9);
            }
            self.relayout();
            return;
        }
        if let Some(window) = &self.window {
            let icon = match self.divider_at(x, y) {
                Some(i) if self.dividers[i].side_by_side => CursorIcon::ColResize,
                Some(_) => CursorIcon::RowResize,
                None if y < self.bar_heights().0 => CursorIcon::Default,
                None => CursorIcon::Text,
            };
            window.set_cursor(icon);
        }
        let Some(press) = &self.press else { return };
        let (id, start, start_side, clicks, reported) = (press.pane, press.point, press.side, press.clicks, press.reported);
        let Some(rect) = self.pane_rects.iter().find(|(p, _)| *p == id).map(|(_, r)| *r) else { return };
        let Some((point, side)) = self.cell_at(id, rect, x, y) else { return };
        if reported {
            let drag = self.panes[&id].term.lock().mode().intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION);
            if drag && point != start {
                self.mouse_report(id, 32, point, true);
            }
            return;
        }
        if point == start && side == start_side && !press.moved {
            return;
        }
        let mut term = self.panes[&id].term.lock();
        match term.selection.as_mut() {
            Some(selection) => selection.update(point, side),
            None => {
                let mut selection = Selection::new(SelectionType::Simple, start, start_side);
                selection.update(point, side);
                term.selection = Some(selection);
            },
        }
        drop(term);
        let _ = clicks;
        if let Some(press) = &mut self.press {
            press.moved = true;
        }
        self.redraw();
    }

    fn mouse_release(&mut self) {
        self.drag_divider = None;
        let Some(press) = self.press.take() else { return };
        if press.reported {
            let (x, y) = self.mouse;
            let rect = self.pane_rects.iter().find(|(p, _)| *p == press.pane).map(|(_, r)| *r);
            let point = rect.and_then(|r| self.cell_at(press.pane, r, x, y)).map_or(press.point, |(p, _)| p);
            self.mouse_report(press.pane, 0, point, false);
            return;
        }
        if press.moved || press.clicks != 1 {
            return;
        }
        // A click with no drag in the command line moves the shell cursor to the click.
        let Some(pane) = self.panes.get(&press.pane) else { return };
        let term = pane.term.lock();
        let Some(region) = Region::new(pane, &term) else { return };
        let target = region.idx(press.point) + if press.side == Side::Right { 1 } else { 0 };
        let last_line = region.point(region.end).line;
        if target < 0 || press.point.line > last_line {
            return;
        }
        let mut bytes = Vec::new();
        region.arrows(&term, region.cursor, target.min(region.end), &mut bytes);
        drop(term);
        if !bytes.is_empty() {
            self.send(press.pane, bytes);
        }
    }

    fn divider_at(&self, x: f32, y: f32) -> Option<usize> {
        self.dividers.iter().position(|d| {
            let r = d.rect;
            Rect { x: r.x - 3.0, y: r.y - 3.0, w: r.w + 6.0, h: r.h + 6.0 }.contains(x, y)
        })
    }

    fn wheel(&mut self, lines: i32) {
        let (x, y) = self.mouse;
        let Some((id, rect)) = self.pane_at(x, y) else { return };
        let Some(pane) = self.panes.get(&id) else { return };
        let mode = *pane.term.lock().mode();
        if mode.intersects(TermMode::MOUSE_MODE) {
            if let Some((point, _)) = self.cell_at(id, rect, x, y) {
                for _ in 0..lines.abs() {
                    self.mouse_report(id, if lines > 0 { 64 } else { 65 }, point, true);
                }
            }
        } else if mode.contains(TermMode::ALT_SCREEN) {
            let key: &[u8] = if lines > 0 { b"\x1bOA" } else { b"\x1bOB" };
            self.send(id, key.repeat(lines.unsigned_abs() as usize));
        } else {
            pane.term.lock().scroll_display(Scroll::Delta(lines));
        }
        self.redraw();
    }

    fn copy(&mut self) {
        let Some(text) = self.panes.get(&self.focus()).and_then(|p| p.term.lock().selection_to_string()) else { return };
        self.set_clipboard(text);
    }

    fn set_clipboard(&mut self, text: String) {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        if let Some(clipboard) = &mut self.clipboard {
            let _ = clipboard.set_text(text);
        }
    }

    fn paste(&mut self) {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        let Some(text) = self.clipboard.as_mut().and_then(|c| c.get_text().ok()) else { return };
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        let id = self.focus();
        let Some(pane) = self.panes.get(&id) else { return };
        let mut term = pane.term.lock();
        let mut bytes = Region::new(pane, &term).and_then(|r| r.delete_selection(&term)).unwrap_or_default();
        if term.mode().contains(TermMode::BRACKETED_PASTE) {
            bytes.extend_from_slice(b"\x1b[200~");
            bytes.extend_from_slice(text.replace('\x1b', "").as_bytes());
            bytes.extend_from_slice(b"\x1b[201~");
        } else {
            bytes.extend_from_slice(text.as_bytes());
        }
        term.selection = None;
        term.scroll_display(Scroll::Bottom);
        drop(term);
        self.send(id, bytes);
        self.redraw();
    }

    /// Finds a match of the search pattern and selects it.
    /// `advance` starts at the current match. If not, the search starts at the end of the output.
    fn find(&mut self, older: bool, advance: bool) {
        let Some(pattern) = self.search.clone() else { return };
        let Some(pane) = self.panes.get(&self.focus()) else { return };
        let mut term = pane.term.lock();
        let current = term.selection.as_ref().and_then(|s| s.to_range(&term));
        term.selection = None;
        if pattern.is_empty() {
            return;
        }
        let Ok(mut regex) = RegexSearch::new(&pattern) else { return };
        let direction = if older { Direction::Left } else { Direction::Right };
        let origin = match (advance, current) {
            (true, Some(range)) if older => range.start.sub(&*term, Boundary::None, 1),
            (true, Some(range)) => range.end.add(&*term, Boundary::None, 1),
            _ => Point::new(term.bottommost_line(), term.last_column()),
        };
        let side = if older { Side::Right } else { Side::Left };
        if let Some(found) = term.search_next(&mut regex, origin, direction, side, None) {
            let mut selection = Selection::new(SelectionType::Simple, *found.start(), Side::Left);
            selection.update(*found.end(), Side::Right);
            term.selection = Some(selection);
            term.scroll_to_point(*found.start());
        }
    }

    /// Scrolls to the prompt above or below the top line of the view.
    fn jump_prompt(&mut self, up: bool) {
        let Some(pane) = self.panes.get(&self.focus()) else { return };
        let mut term = pane.term.lock();
        let history = term.grid().history_size() as i32;
        let offset = term.grid().display_offset() as i32;
        let top = -offset;
        let lines = pane.blocks.iter().map(|b| b.line - history);
        let target = if up { lines.filter(|l| *l < top).max() } else { lines.filter(|l| *l > top).min() };
        match target {
            Some(line) if line < 0 => term.scroll_display(Scroll::Delta(-line - offset)),
            _ if !up => term.scroll_display(Scroll::Bottom),
            _ => {},
        }
        drop(term);
        self.redraw();
    }

    /// Opens the URL or the file path at `point`. Returns `false` if there is no link.
    fn open_link(&self, id: usize, point: Point) -> bool {
        let Some(pane) = self.panes.get(&id) else { return false };
        let term = pane.term.lock();
        let grid = term.grid();
        let text = match grid[point.line][point.column].hyperlink() {
            Some(link) => link.uri().to_string(),
            None => {
                let mut chars = Vec::new();
                let mut index = 0;
                for col in 0..grid.columns() {
                    let cell = &grid[point.line][Column(col)];
                    if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                        continue;
                    }
                    if col <= point.column.0 {
                        index = chars.len();
                    }
                    chars.push(cell.c);
                }
                token_at(&chars, index)
            },
        };
        drop(term);
        let Some(target) = resolve_link(&text, pane.cwd.as_deref()) else { return false };
        // A program or a script does not start from a click. Explorer shows the file.
        let risky = ["exe", "bat", "cmd", "com", "ps1", "msi", "vbs", "js", "lnk", "scr"];
        let extension = std::path::Path::new(&target).extension().map(|e| e.to_string_lossy().to_ascii_lowercase());
        let is_url = target.starts_with("http");
        let mut command = std::process::Command::new("explorer.exe");
        if !is_url && extension.is_some_and(|e| risky.contains(&e.as_str())) {
            command.arg(format!("/select,{target}"));
        } else {
            command.arg(&target);
        }
        command.spawn().is_ok()
    }

    fn set_font(&mut self, font_px: f32) {
        if let Some(gpu) = self.gpu.as_mut() {
            gpu.set_font_px(font_px);
        }
        // Force a new size for each pane.
        for pane in self.panes.values_mut() {
            pane.cols = 0;
        }
        self.relayout();
    }

    fn default_font_px(&self) -> f32 {
        FONT_PX * self.window.as_ref().map_or(1.0, |w| w.scale_factor() as f32)
    }

    fn key(&mut self, event: KeyEvent, event_loop: &ActiveEventLoop) {
        if event.state != ElementState::Pressed {
            return;
        }
        let (ctrl, shift, alt) = (self.mods.control_key(), self.mods.shift_key(), self.mods.alt_key());
        let key = event.logical_key.clone();

        if self.search.is_some() {
            match &key {
                Key::Named(NamedKey::Escape) => {
                    self.search = None;
                    self.relayout();
                },
                Key::Named(NamedKey::Enter) => self.find(!shift, true),
                Key::Named(NamedKey::Backspace) => {
                    self.search.as_mut().unwrap().pop();
                    self.find(true, false);
                },
                _ => {
                    if let (Some(text), false) = (&event.text, ctrl) {
                        self.search.as_mut().unwrap().push_str(text);
                        self.find(true, false);
                    }
                },
            }
            self.redraw();
            return;
        }

        let character = match &key {
            Key::Character(c) => c.to_lowercase(),
            _ => String::new(),
        };
        let focus = self.tabs[self.active].focus;
        let has_selection =
            self.panes.get(&focus).is_some_and(|p| p.term.lock().selection.as_ref().is_some_and(|s| !s.is_empty()));

        // bebo commands.
        let handled = match (ctrl, shift, alt, character.as_str(), &key) {
            (true, true, false, "t", _) => self.new_tab(),
            (true, true, false, "w", _) => self.close_pane(focus, event_loop),
            (true, true, false, "d", _) => self.split(true),
            (true, true, false, "e", _) => self.split(false),
            (true, true, false, "r", _) => self.restart(focus),
            (true, true, false, "f", _) => {
                self.search = Some(String::new());
                self.relayout();
            },
            (true, true, false, "c", _) => self.copy(),
            (true, _, false, "v", _) => self.paste(),
            (true, false, false, "c", _) if has_selection => {
                self.copy();
                if let Some(pane) = self.panes.get(&focus) {
                    pane.term.lock().selection = None;
                }
                self.redraw();
            },
            (true, false, false, "=" | "+", _) => self.set_font(self.gpu.as_ref().map_or(FONT_PX, |g| g.font_px + 1.0)),
            (true, false, false, "-", _) => self.set_font(self.gpu.as_ref().map_or(FONT_PX, |g| g.font_px - 1.0)),
            (true, false, false, "0", _) => self.set_font(self.default_font_px()),
            (true, _, false, _, Key::Named(NamedKey::Tab)) => {
                let n = self.tabs.len();
                self.active = if shift { (self.active + n - 1) % n } else { (self.active + 1) % n };
                self.relayout();
            },
            (false, true, false, _, Key::Named(NamedKey::PageUp | NamedKey::PageDown)) => {
                let scroll = if key == Key::Named(NamedKey::PageUp) { Scroll::PageUp } else { Scroll::PageDown };
                if let Some(pane) = self.panes.get(&focus) {
                    pane.term.lock().scroll_display(scroll);
                }
                self.redraw();
            },
            (true, true, false, _, Key::Named(NamedKey::ArrowUp | NamedKey::ArrowDown)) => {
                self.jump_prompt(key == Key::Named(NamedKey::ArrowUp));
            },
            (false, false, true, _, Key::Named(NamedKey::ArrowLeft | NamedKey::ArrowUp | NamedKey::ArrowRight | NamedKey::ArrowDown)) => {
                // ponytail: focus moves in tree order, not by direction on the screen.
                let mut leaves = Vec::new();
                self.tabs[self.active].root.leaves(&mut leaves);
                let n = leaves.len();
                let i = leaves.iter().position(|l| *l == focus).unwrap_or(0);
                let back = matches!(key, Key::Named(NamedKey::ArrowLeft | NamedKey::ArrowUp));
                self.tabs[self.active].focus = leaves[if back { (i + n - 1) % n } else { (i + 1) % n }];
                self.relayout();
            },
            _ => {
                self.type_key(&key, event.text.as_deref());
                return;
            },
        };
        let () = handled;
    }

    /// Sends a key to the shell. If a selection is in the command line, a text key replaces it.
    fn type_key(&mut self, key: &Key, text: Option<&str>) {
        let (ctrl, alt) = (self.mods.control_key(), self.mods.alt_key());
        let id = self.focus();
        let Some(pane) = self.panes.get_mut(&id) else { return };
        let mut term = pane.term.lock();
        let Some(bytes) = encode_key(key, text, self.mods, term.mode().contains(TermMode::APP_CURSOR)) else { return };

        let plain = !ctrl && !alt;
        let is_text = plain && matches!(key, Key::Character(_) | Key::Named(NamedKey::Space));
        let is_delete = plain && matches!(key, Key::Named(NamedKey::Backspace | NamedKey::Delete));
        let edit = if is_text || is_delete {
            Region::new(pane, &term).and_then(|r| r.delete_selection(&term))
        } else {
            None
        };
        let out = match edit {
            Some(mut out) => {
                if is_text {
                    out.extend(bytes);
                }
                out
            },
            None => bytes,
        };
        term.selection = None;
        term.scroll_display(Scroll::Bottom);
        drop(term);
        if matches!(key, Key::Named(NamedKey::Enter)) {
            pane.input_start = None;
        }
        let _ = pane.tx.send(Msg::Input(Cow::Owned(out)));
        self.redraw();
    }

    fn term_event(&mut self, id: usize, generation: u32, event: Event, event_loop: &ActiveEventLoop) {
        let gpu = self.gpu.as_ref();
        let Some(pane) = self.panes.get_mut(&id) else { return };
        match event {
            Event::Wakeup => {
                if pane.await_input {
                    pane.await_input = false;
                    let term = pane.term.lock();
                    let cursor = term.grid().cursor.point;
                    let line = cursor.line.0 + term.grid().history_size() as i32;
                    pane.input_start = Some((line, cursor.column.0));
                    // A clear-screen makes the line numbers start again. Remove the blocks that are not above this prompt.
                    pane.blocks.retain(|b| b.line < line);
                    if pane.blocks.len() >= 2000 {
                        pane.blocks.remove(0);
                    }
                    pane.blocks.push(Block { line, exec_at: None, exit: None, duration: None });
                }
                self.redraw();
            },
            Event::Title(title) => {
                pane.title = title;
                if id == self.focus() {
                    self.relayout();
                }
            },
            Event::PtyWrite(text) => self.send(id, text.into_bytes()),
            Event::ColorRequest(index, format) => {
                let color = pane.term.lock().colors()[index];
                let [r, g, b] = color.map_or(palette(index), |c| [c.r, c.g, c.b]);
                let text = format(alacritty_terminal::vte::ansi::Rgb { r, g, b });
                self.send(id, text.into_bytes());
            },
            Event::TextAreaSizeRequest(format) => {
                let text = format(Self::window_size(pane.cols, pane.rows, gpu));
                self.send(id, text.into_bytes());
            },
            Event::ClipboardStore(_, text) => self.set_clipboard(text),
            Event::ChildExit(_) | Event::Exit if generation == pane.generation => self.close_pane(id, event_loop),
            _ => {},
        }
    }

    fn mark(&mut self, id: usize, mark: Mark) {
        let Some(pane) = self.panes.get_mut(&id) else { return };
        match mark {
            Mark::InputStart => {
                pane.await_input = true;
                if std::env::var_os("BEBO_TIMING").is_some() {
                    render::tick(self.start, "shell prompt");
                    let _ = std::fs::write(std::env::temp_dir().join("bebo-timing.txt"), &*render::TIMING.lock().unwrap());
                }
            },
            Mark::Exec => {
                pane.input_start = None;
                if let Some(block) = pane.blocks.last_mut() {
                    block.exec_at = Some(Instant::now());
                }
            },
            Mark::Done(code) => {
                if let Some(block) = pane.blocks.last_mut()
                    && let (Some(exec_at), None) = (block.exec_at, block.exit)
                {
                    block.exit = Some(code);
                    block.duration = Some(exec_at.elapsed());
                }
            },
            Mark::Cwd(path) => {
                let path = PathBuf::from(path);
                // The tab title is the folder name.
                pane.title = path.file_name().map_or(path.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned());
                pane.cwd = Some(path);
                self.relayout();
            },
            Mark::Restart => self.restart(id),
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = Window::default_attributes().with_title("bebo").with_inner_size(LogicalSize::new(1000.0, 640.0));
        let window = Arc::new(event_loop.create_window(attributes).expect("create window"));
        // The window shows immediately with the background color. The GPU frame comes when the device is ready.
        paint_background(&window);
        self.window = Some(window);
        render::tick(self.start, "window");
        self.start_gpu();
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Term(id, generation, event) => self.term_event(id, generation, event, event_loop),
            UserEvent::Mark(id, mark) => self.mark(id, mark),
            UserEvent::GpuReady => self.start_gpu(),
        }
    }
    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(gpu) = self.gpu.as_mut() {
                    gpu.resize(size.width, size.height);
                }
                self.relayout();
            },
            WindowEvent::ScaleFactorChanged { .. } => self.set_font(self.default_font_px()),
            WindowEvent::RedrawRequested if self.gpu.is_none() => {
                if let Some(window) = &self.window {
                    paint_background(window);
                }
            },
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::ModifiersChanged(mods) => self.mods = mods.state(),
            WindowEvent::KeyboardInput { event, .. } => self.key(event, event_loop),
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse = (position.x as f32, position.y as f32);
                self.mouse_move();
            },
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => match state {
                ElementState::Pressed => self.mouse_press(),
                ElementState::Released => self.mouse_release(),
            },
            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => (y * 3.0).round() as i32,
                    MouseScrollDelta::PixelDelta(p) => (p.y as f32 / self.gpu.as_ref().map_or(18.0, |g| g.cell_h)) as i32,
                };
                if lines != 0 {
                    self.wheel(lines);
                }
            },
            WindowEvent::Focused(_) => self.redraw(),
            _ => {},
        }
    }
}

fn draw_pane(frame: &mut Frame, pane: &Pane, rect: Rect, focused: bool, cw: f32, ch: f32) {
    let term = pane.term.lock();
    let content = term.renderable_content();
    let offset = content.display_offset as i32;
    let (ox, oy) = (rect.x + PAD, rect.y + PAD);
    let clip = [rect.x, rect.y, rect.x + rect.w, rect.y + rect.h];
    let cursor = content.cursor;
    let cursor_visible = cursor.shape != CursorShape::Hidden && offset == 0;

    let mut row = i32::MIN;
    let mut spans: Vec<Span> = Vec::new();
    let flush = |row: i32, spans: &mut Vec<Span>, frame: &mut Frame| {
        while let Some(last) = spans.last_mut() {
            let trimmed = last.text.trim_end_matches(' ').len();
            last.text.truncate(trimmed);
            if !last.text.is_empty() {
                break;
            }
            spans.pop();
        }
        if !spans.is_empty() {
            frame.rows.push(Row { x: ox, y: oy + row as f32 * ch, clip, spans: std::mem::take(spans) });
        }
    };

    for cell in content.display_iter {
        let point = cell.point;
        let line = point.line.0 + offset;
        if line != row {
            if row != i32::MIN {
                flush(row, &mut spans, frame);
            }
            row = line;
        }
        let flags = cell.flags;
        if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
            continue;
        }
        let mut fg = resolve(cell.fg, content.colors);
        let mut bg = resolve(cell.bg, content.colors);
        if flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if flags.contains(Flags::DIM) {
            fg = dim(fg);
        }
        if content.selection.is_some_and(|s| s.contains(point)) {
            bg = SELECTION;
        }
        let (x, y) = (ox + point.column.0 as f32 * cw, oy + line as f32 * ch);
        let w = if flags.contains(Flags::WIDE_CHAR) { cw * 2.0 } else { cw };
        if cursor_visible && point == cursor.point {
            match cursor.shape {
                CursorShape::Block if focused => {
                    bg = CURSOR;
                    fg = BG;
                },
                CursorShape::Beam if focused => frame.overlay.push(Quad { x, y, w: 2.0, h: ch, color: rgba(CURSOR) }),
                CursorShape::Underline if focused => {
                    frame.overlay.push(Quad { x, y: y + ch - 2.0, w, h: 2.0, color: rgba(CURSOR) })
                },
                _ => {
                    // No focus: draw the outline only.
                    for (qx, qy, qw, qh) in [(x, y, w, 1.0), (x, y + ch - 1.0, w, 1.0), (x, y, 1.0, ch), (x + w - 1.0, y, 1.0, ch)] {
                        frame.overlay.push(Quad { x: qx, y: qy, w: qw, h: qh, color: rgba(CURSOR) });
                    }
                },
            }
        }
        if bg != BG {
            // Extend the last rectangle if it has the same color and touches this cell.
            match frame.quads.last_mut() {
                Some(q) if q.y == y && q.color == rgba(bg) && (q.x + q.w - x).abs() < 0.5 => q.w += w,
                _ => frame.quads.push(Quad { x, y, w, h: ch, color: rgba(bg) }),
            }
        }
        if flags.intersects(Flags::ALL_UNDERLINES) {
            frame.overlay.push(Quad { x, y: y + ch - 2.0, w, h: 1.0, color: rgba(fg) });
        }
        if flags.contains(Flags::STRIKEOUT) {
            frame.overlay.push(Quad { x, y: y + ch / 2.0, w, h: 1.0, color: rgba(fg) });
        }

        let c = if flags.contains(Flags::HIDDEN) || cell.c == '\t' { ' ' } else { cell.c };
        let (bold, italic) = (flags.contains(Flags::BOLD), flags.contains(Flags::ITALIC));
        match spans.last_mut() {
            Some(s) if s.color == fg && s.bold == bold && s.italic == italic => s.text.push(c),
            _ => spans.push(Span { text: c.to_string(), color: fg, bold, italic }),
        }
        if let Some(extra) = cell.zerowidth() {
            spans.last_mut().unwrap().text.extend(extra);
        }
    }
    if row != i32::MIN {
        flush(row, &mut spans, frame);
    }

    // Command blocks: a separator above each prompt, a red bar for a failed command, and the duration.
    let history = term.grid().history_size() as i32;
    // ponytail: when the scrollback is full, old lines drop and the block lines are wrong. Blocks are off then.
    if history >= 10_000 || term.mode().contains(TermMode::ALT_SCREEN) {
        return;
    }
    let rows = pane.rows as i32;
    for (i, block) in pane.blocks.iter().enumerate() {
        let top = block.line - history + offset;
        let end = pane.blocks.get(i + 1).map_or(rows, |next| next.line - history + offset).min(rows);
        if end <= 0 || top >= rows {
            continue;
        }
        if top > 0 && i > 0 {
            frame.quads.push(Quad { x: rect.x, y: oy + top as f32 * ch - 1.0, w: rect.w, h: 1.0, color: rgba(SEPARATOR) });
        }
        let first = top.max(0);
        if block.exit.is_some_and(|code| code != 0) {
            frame.overlay.push(Quad { x: rect.x + 1.0, y: oy + first as f32 * ch, w: 3.0, h: (end - first) as f32 * ch, color: rgba(ERROR) });
        }
        if let (Some(duration), true) = (block.duration, top >= 0)
            && duration >= Duration::from_secs(1)
        {
            let secs = duration.as_secs();
            let mut label = if secs < 60 { format!("{:.1}s", duration.as_secs_f32()) } else { format!("{}m{}s", secs / 60, secs % 60) };
            if let Some(code) = block.exit.filter(|code| *code != 0) {
                label = format!("exit {code}  {label}");
            }
            let x = rect.x + rect.w - PAD - label.chars().count() as f32 * cw;
            let color = if block.exit == Some(0) { dim(FG) } else { ERROR };
            frame.rows.push(Row { x, y: oy + top as f32 * ch, clip, spans: vec![Span { text: label, color, bold: false, italic: false }] });
        }
    }
}

/// Fills the window with the background color through GDI. bebo uses this before the GPU is ready.
fn paint_background(window: &Window) {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::{CreateSolidBrush, DeleteObject, FillRect, GetDC, ReleaseDC};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Ok(handle) = window.window_handle() else { return };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else { return };
    let hwnd = handle.hwnd.get() as *mut core::ffi::c_void;
    let size = window.inner_size();
    let rect = RECT { left: 0, top: 0, right: size.width as i32, bottom: size.height as i32 };
    // SAFETY: `hwnd` is the handle of a window that is alive. The DC and the brush are released here.
    unsafe {
        let dc = GetDC(hwnd);
        let brush = CreateSolidBrush(BG[0] as u32 | (BG[1] as u32) << 8 | (BG[2] as u32) << 16);
        FillRect(dc, &rect, brush);
        DeleteObject(brush);
        ReleaseDC(hwnd, dc);
    }
}

/// The word at `index`. A space, a quote, or a bracket ends the word.
fn token_at(chars: &[char], index: usize) -> String {
    let stop = |c: char| c.is_whitespace() || "\"'<>()[]{}|".contains(c);
    if chars.get(index).is_none_or(|c| stop(*c)) {
        return String::new();
    }
    let start = chars[..index].iter().rposition(|c| stop(*c)).map_or(0, |i| i + 1);
    let end = chars[index..].iter().position(|c| stop(*c)).map_or(chars.len(), |i| index + i);
    let token: String = chars[start..end].iter().collect();
    token.trim_end_matches(['.', ',', ';', ':', '!', '?']).to_string()
}

/// Returns the target to open: an http(s) URL, or a path that exists. `file.rs:12:5` gives `file.rs`.
fn resolve_link(text: &str, cwd: Option<&std::path::Path>) -> Option<String> {
    if text.starts_with("http://") || text.starts_with("https://") {
        return Some(text.to_string());
    }
    let mut path = text.strip_prefix("file:///").unwrap_or(text);
    for _ in 0..2 {
        match path.rsplit_once(':') {
            Some((head, tail)) if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) => path = head,
            _ => break,
        }
    }
    if path.is_empty() {
        return None;
    }
    let path = PathBuf::from(path);
    let path = if path.is_absolute() { path } else { cwd?.join(path) };
    path.exists().then(|| path.to_string_lossy().into_owned())
}

fn rgba(c: [u8; 3]) -> [u8; 4] {
    [c[0], c[1], c[2], 255]
}

fn dim(c: [u8; 3]) -> [u8; 3] {
    c.map(|v| (v as f32 * 0.66) as u8)
}

fn palette(index: usize) -> [u8; 3] {
    const BASE: [[u8; 3]; 16] = [
        [0x1d, 0x1f, 0x28],
        [0xe0, 0x6c, 0x75],
        [0x98, 0xc3, 0x79],
        [0xe5, 0xc0, 0x7b],
        [0x61, 0xaf, 0xef],
        [0xc6, 0x78, 0xdd],
        [0x56, 0xb6, 0xc2],
        [0xab, 0xb2, 0xbf],
        [0x5c, 0x63, 0x70],
        [0xf0, 0x7f, 0x89],
        [0xa9, 0xd8, 0x8a],
        [0xf2, 0xd0, 0x8c],
        [0x74, 0xbf, 0xff],
        [0xd8, 0x8c, 0xee],
        [0x66, 0xcc, 0xd8],
        [0xff, 0xff, 0xff],
    ];
    match index {
        0..=15 => BASE[index],
        16..=231 => {
            let i = index - 16;
            let level = |n: usize| if n == 0 { 0 } else { (55 + 40 * n) as u8 };
            [level(i / 36), level(i / 6 % 6), level(i % 6)]
        },
        232..=255 => [(8 + 10 * (index - 232)) as u8; 3],
        257 | 268 => BG,
        258 => CURSOR,
        259..=266 => dim(BASE[index - 259]),
        _ => FG,
    }
}

fn resolve(color: Color, colors: &Colors) -> [u8; 3] {
    let index = match color {
        Color::Spec(rgb) => return [rgb.r, rgb.g, rgb.b],
        Color::Named(name) => name as usize,
        Color::Indexed(i) => i as usize,
    };
    colors[index].map_or(palette(index), |c| [c.r, c.g, c.b])
}

/// Changes a key press into the bytes that a terminal sends.
fn encode_key(key: &Key, text: Option<&str>, mods: ModifiersState, app_cursor: bool) -> Option<Vec<u8>> {
    let (ctrl, shift, alt) = (mods.control_key(), mods.shift_key(), mods.alt_key());
    let m = 1 + shift as u8 + 2 * alt as u8 + 4 * ctrl as u8;
    let csi = |c: char| match (m, app_cursor) {
        (1, true) => format!("\x1bO{c}"),
        (1, false) => format!("\x1b[{c}"),
        _ => format!("\x1b[1;{m}{c}"),
    };
    let tilde = |n: u8| if m == 1 { format!("\x1b[{n}~") } else { format!("\x1b[{n};{m}~") };
    let esc = if alt { "\x1b" } else { "" };

    let out = match key {
        Key::Named(named) => match named {
            NamedKey::Enter => format!("{esc}\r"),
            NamedKey::Backspace => format!("{esc}{}", if ctrl { "\x08" } else { "\x7f" }),
            NamedKey::Tab => if shift { "\x1b[Z".into() } else { "\t".into() },
            NamedKey::Escape => "\x1b".into(),
            NamedKey::Space => format!("{esc}{}", if ctrl { "\0" } else { " " }),
            NamedKey::ArrowUp => csi('A'),
            NamedKey::ArrowDown => csi('B'),
            NamedKey::ArrowRight => csi('C'),
            NamedKey::ArrowLeft => csi('D'),
            NamedKey::Home => csi('H'),
            NamedKey::End => csi('F'),
            NamedKey::Insert => tilde(2),
            NamedKey::Delete => tilde(3),
            NamedKey::PageUp => tilde(5),
            NamedKey::PageDown => tilde(6),
            NamedKey::F1 => "\x1bOP".into(),
            NamedKey::F2 => "\x1bOQ".into(),
            NamedKey::F3 => "\x1bOR".into(),
            NamedKey::F4 => "\x1bOS".into(),
            NamedKey::F5 => tilde(15),
            NamedKey::F6 => tilde(17),
            NamedKey::F7 => tilde(18),
            NamedKey::F8 => tilde(19),
            NamedKey::F9 => tilde(20),
            NamedKey::F10 => tilde(21),
            NamedKey::F11 => tilde(23),
            NamedKey::F12 => tilde(24),
            _ => return None,
        },
        Key::Character(c) => {
            let first = c.chars().next()?;
            if ctrl && alt && text.is_some() {
                // AltGr on some keyboard layouts is Ctrl+Alt. Send the text.
                text?.to_string()
            } else if ctrl && c.len() == 1 && (first.is_ascii_alphabetic() || "[\\]^_@".contains(first)) {
                format!("{esc}{}", ((first.to_ascii_uppercase() as u8) & 0x1f) as char)
            } else {
                format!("{esc}{}", text.unwrap_or(c))
            }
        },
        _ => return None,
    };
    Some(out.into_bytes())
}

fn main() {
    let start = Instant::now();
    let event_loop = EventLoop::<UserEvent>::with_user_event().build().expect("event loop");
    // Start the GPU on a second thread now. It is the slowest step of the start.
    let (gpu_tx, gpu_rx) = std::sync::mpsc::channel();
    let gpu_proxy = event_loop.create_proxy();
    std::thread::spawn(move || {
        let _ = gpu_tx.send(render::init_device(start));
        let _ = gpu_proxy.send_event(UserEvent::GpuReady);
    });
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App {
        proxy: event_loop.create_proxy(),
        start,
        window: None,
        gpu: None,
        panes: HashMap::new(),
        tabs: Vec::new(),
        active: 0,
        next_id: 0,
        pane_rects: Vec::new(),
        dividers: Vec::new(),
        mods: ModifiersState::empty(),
        mouse: (0.0, 0.0),
        press: None,
        drag_divider: None,
        last_click: (start, Point::new(Line(0), Column(0)), 0),
        search: None,
        clipboard: None,
        gpu_rx,
    };
    // Start the shell before the window and the GPU. The shell starts while the GPU starts.
    render::tick(start, "event loop");
    app.new_tab();
    render::tick(start, "shell spawned");
    event_loop.run_app(&mut app).expect("run");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_encode_as_terminal_bytes() {
        let none = ModifiersState::empty();
        let enc = |key: Key, text: Option<&str>, mods| encode_key(&key, text, mods, false).unwrap();
        assert_eq!(enc(Key::Character("c".into()), None, ModifiersState::CONTROL), b"\x03");
        assert_eq!(enc(Key::Named(NamedKey::ArrowLeft), None, none), b"\x1b[D");
        assert_eq!(enc(Key::Named(NamedKey::ArrowRight), None, ModifiersState::CONTROL), b"\x1b[1;5C");
        assert_eq!(enc(Key::Character("a".into()), Some("a"), ModifiersState::ALT), b"\x1ba");
        assert_eq!(encode_key(&Key::Named(NamedKey::ArrowUp), None, none, true).unwrap(), b"\x1bOA");
    }

    #[test]
    fn links_come_from_the_word_at_the_click() {
        let line: Vec<char> = r"see (https://example.com/a?b=1). and src\main.rs:12:5, ok".chars().collect();
        assert_eq!(token_at(&line, 8), "https://example.com/a?b=1");
        assert_eq!(token_at(&line, 3), "");
        assert_eq!(token_at(&line, 40), r"src\main.rs:12:5");
        assert_eq!(resolve_link("https://example.com/a", None).as_deref(), Some("https://example.com/a"));
        let cwd = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(resolve_link(r"src\main.rs:12:5", Some(cwd)).unwrap().ends_with("main.rs"));
        assert_eq!(resolve_link("no-such-file.txt", Some(cwd)), None);
        assert_eq!(resolve_link("javascript:alert(1)", Some(cwd)), None);
    }

    #[test]
    fn tree_split_and_remove() {
        let mut root = Node::Leaf(1);
        root.split(1, 2, true);
        root.split(2, 3, false);
        let mut leaves = Vec::new();
        root.leaves(&mut leaves);
        assert_eq!(leaves, [1, 2, 3]);
        let root = root.remove(2).unwrap();
        let mut leaves = Vec::new();
        root.leaves(&mut leaves);
        assert_eq!(leaves, [1, 3]);
        assert!(Node::Leaf(1).remove(1).is_none());
    }
}
