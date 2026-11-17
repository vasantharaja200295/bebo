#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! bebo: a GPU terminal emulator. See docs/PLAN.md.

mod pty;
mod render;
mod theme;
mod ui;

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, WindowSize};
use alacritty_terminal::event_loop::{EventLoopSender, Msg};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::search::{RegexIter, RegexSearch};
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, Processor, StdSyncHandler};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};
use winit::platform::windows::{IconExtWindows, WindowAttributesExtWindows};
use winit::window::{CursorIcon, Icon, ResizeDirection, Window, WindowId};

use pty::{Mark, Profile, Proxy, SharedTerm, TermSize, UserEvent};
use render::Gpu;
use theme::{Rgb, Theme};

/// Counts the heap memory for the profiler. The cost is one atomic add for each allocation.
#[global_allocator]
static HEAP: render::prof::Counter = render::prof::Counter;

const FONT_PX: f32 = 13.0;
/// The limits of the terminal font size in logical pixels, for the keys and for the settings page.
const FONT_MIN: f32 = 8.0;
const FONT_MAX: f32 = 32.0;

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
    profile: Profile,
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
    /// Grid line at the top row of the view. `draw` sets it.
    view_top: i32,
    /// Pixel position of the cursor cell. `draw` sets it.
    cursor_px: Option<(f32, f32)>,
    /// Message at the prompt after a restart.
    badge: Option<Badge>,
    /// After a resize, bebo asks PowerShell at this time to draw the prompt again.
    /// The flag is `false` if a resize in this series changed the width. ConPTY then draws the prompt correctly.
    prompt_fix: Option<(Instant, bool)>,
}

/// A prompt, its command, and the output of the command.
struct Block {
    /// Line of the prompt: grid line + history size.
    line: i32,
    command: String,
    exec_at: Option<Instant>,
    exit: Option<i32>,
    duration: Option<Duration>,
}

/// Exit code of a program that Ctrl+C stopped (STATUS_CONTROL_C_EXIT).
const CTRL_C_EXIT: i32 = 0xC000013Au32 as i32;

impl Block {
    /// The command failed. A stop with Ctrl+C is not a failure.
    fn failed(&self) -> bool {
        self.exit.is_some_and(|code| code != 0 && code != CTRL_C_EXIT)
    }
}

struct Badge {
    text: String,
    lost: bool,
    /// The badge goes away at this time. `None`: it stays until the first key.
    until: Option<Instant>,
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
    /// Name from the rename command.
    name: Option<String>,
    /// This tab shows the settings page. It has no pane: `focus` is `usize::MAX`.
    settings: bool,
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

/// An action of bebo. The palette, the colon popup, and the keys run these.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Cmd {
    NewTab,
    RenameTab,
    NextTab,
    SplitDown,
    SplitRight,
    ClosePane,
    Find,
    Restart,
    RestartForce,
    RestartClean,
    Clear,
    Layout,
    Theme(bool),
    Palette,
    Profile(Profile),
    /// Opens a new bebo window as administrator.
    Admin(Profile),
    Copy,
    Paste,
    Settings,
}

struct Entry {
    category: &'static str,
    title: String,
    keys: &'static str,
    cmd: Cmd,
    current: bool,
}

/// Colon commands: name, description, keys, action.
const COLON: [(&str, &str, &str, Cmd); 12] = [
    ("tab", "New tab", "Ctrl+Shift+T", Cmd::NewTab),
    ("hsplit", "Split top and bottom", "Alt+Shift+-", Cmd::SplitDown),
    ("vsplit", "Split left and right", "Alt+Shift++", Cmd::SplitRight),
    ("find", "Find in output", "Ctrl+Shift+F", Cmd::Find),
    ("restart", "Restart the shell", "Ctrl+Shift+R", Cmd::Restart),
    ("clear", "Clear output", "Ctrl+K", Cmd::Clear),
    ("layout", "Prompt at top or bottom", "", Cmd::Layout),
    ("profile", "Open a different shell", "", Cmd::Palette),
    ("theme", "One Dark or One Light", "", Cmd::Palette),
    ("rename", "Rename this tab", "", Cmd::RenameTab),
    ("palette", "All actions", "Ctrl+Shift+P", Cmd::Palette),
    ("settings", "Open the settings", "Ctrl+,", Cmd::Settings),
];

const RESTART_ARGS: [(&str, &str); 3] = [
    ("restore", "Default. Keep scrollback and folder."),
    ("force", "Skip the confirm. Stop running jobs."),
    ("clean", "Start fresh. Content is lost."),
];

/// Rows of the colon popup for the typed text: (name, description, keys).
fn colon_items(text: &str) -> Vec<(String, &'static str, &'static str)> {
    if text == "restart" || text.starts_with("restart ") {
        let last = if text.ends_with(' ') || text == "restart" { "" } else { text.rsplit(' ').next().unwrap_or("") };
        return RESTART_ARGS.iter().filter(|(n, _)| n.starts_with(last)).map(|(n, d)| (n.to_string(), *d, "")).collect();
    }
    if let Some(name) = text.strip_prefix("rename ") {
        let hint = if name.trim().is_empty() { "Type the new name. Enter with no name removes the name." } else { "Enter gives this name to the tab" };
        return vec![(":rename".to_string(), hint, "")];
    }
    COLON.iter().filter(|(n, ..)| n.starts_with(text)).map(|(n, d, k, _)| (format!(":{n}"), *d, *k)).collect()
}

/// Positions of the characters of `query` in `text`, in order. `None` if there is no match.
fn fuzzy(text: &str, query: &str) -> Option<Vec<usize>> {
    let mut positions = Vec::new();
    let mut chars = text.char_indices().map(|(i, c)| (i, c.to_ascii_lowercase()));
    for q in query.chars().filter(|c| !c.is_whitespace()).map(|c| c.to_ascii_lowercase()) {
        positions.push(chars.find(|(_, c)| *c == q)?.0);
    }
    Some(positions)
}

#[derive(Default)]
struct Palette {
    query: String,
    selected: usize,
    /// The box asks for a new tab name.
    rename: bool,
}

#[derive(Default)]
struct Colon {
    text: String,
    selected: usize,
}

#[derive(Default)]
struct Find {
    query: String,
    case: bool,
    word: bool,
    regex: bool,
    /// Start and end cell of each match, from top to bottom.
    matches: Vec<(Point, Point)>,
    current: usize,
}

impl Find {
    fn pattern(&self) -> String {
        let mut base = self.query.clone();
        if !self.regex {
            base = base.chars().flat_map(|c| if r"\.+*?()|[]{}^$#&-~".contains(c) { vec!['\\', c] } else { vec![c] }).collect();
        }
        if self.word {
            base = format!(r"(?-u:\b)(?:{base})(?-u:\b)");
        }
        if self.case { format!("(?-i:{base})") } else { format!("(?i:{base})") }
    }
}

/// A part of the window that reacts to the mouse. `draw` makes the list.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Hit {
    TitleDrag,
    Tab(usize),
    TabClose(usize),
    NewTab,
    Menu,
    MenuItem(usize),
    /// The administrator button of a profile in the menu.
    MenuAdmin(usize),
    /// A row of the font list.
    Font(usize),
    WinMin,
    WinMax,
    WinClose,
    PaletteItem(usize),
    ColonItem(usize),
    FindToggle(u8),
    FindPrev,
    FindNext,
    FindClose,
    Minimap(usize),
    /// A control of the settings page: (row, choice).
    Setting(u8, u8),
    SettingsPage(usize),
    /// A popup. A click here does nothing and does not go to the terminal.
    Block,
}

/// Adds the four sizes of the dark and the light version of an icon. The light version has the id + 10.
macro_rules! add_icon {
    ($gpu:expr, $id:expr, $name:literal) => {
        $gpu.add_image($id, include_bytes!(concat!("../assets/", $name, "-dark-16.png")));
        $gpu.add_image($id, include_bytes!(concat!("../assets/", $name, "-dark-20.png")));
        $gpu.add_image($id, include_bytes!(concat!("../assets/", $name, "-dark-24.png")));
        $gpu.add_image($id, include_bytes!(concat!("../assets/", $name, "-dark-32.png")));
        $gpu.add_image($id + 10, include_bytes!(concat!("../assets/", $name, "-light-16.png")));
        $gpu.add_image($id + 10, include_bytes!(concat!("../assets/", $name, "-light-20.png")));
        $gpu.add_image($id + 10, include_bytes!(concat!("../assets/", $name, "-light-24.png")));
        $gpu.add_image($id + 10, include_bytes!(concat!("../assets/", $name, "-light-32.png")));
    };
}

/// Number of rows in the font list of the settings page.
const FONT_ROWS: usize = 10;

// Keys of the animation values.
const A_PALETTE: u32 = 1;
const A_MENU: u32 = 2;
const A_COLON: u32 = 3;
const A_FIND: u32 = 4;
const A_FONTS: u32 = 5;
const A_CONFIRM: u32 = 6;
/// Settings switch `n` uses `A_SWITCH + n`.
const A_SWITCH: u32 = 50;
/// Pane `n` uses `A_CARET + 2 * n` for the caret x position and the next key for the caret row.
const A_CARET: u32 = 1000;

/// Values that move to a target in a short time. `draw` sets `dt` for each frame.
#[derive(Default)]
struct Anim {
    values: HashMap<u32, f32>,
    /// Seconds from the last frame.
    dt: f32,
    /// The setting "Animations".
    on: bool,
    /// A value moved in this frame. `draw` then asks for the next frame.
    active: bool,
    /// The user pressed a key a moment ago. Only then the caret moves smoothly: not at the start and not for shell output.
    caret_live: bool,
}

impl Anim {
    /// Moves the value of `key` in the direction of `target` and returns it. The first call returns `target`.
    /// The value stops when it is nearer than `near` to the target.
    /// The value goes 63 % of the distance in 18 ms, and stops after about 70 ms. The typing speed has no effect.
    fn to(&mut self, key: u32, target: f32, near: f32) -> f32 {
        let value = self.values.entry(key).or_insert(target);
        if !self.on || (target - *value).abs() < near {
            *value = target;
        } else {
            *value += (target - *value) * (1.0 - (-self.dt / 0.018).exp());
            self.active = true;
        }
        *value
    }

    fn set(&mut self, key: u32, value: f32) {
        self.values.insert(key, value);
    }
}

/// Sizes in pixels for the current scale and font.
#[derive(Clone, Copy)]
struct Metrics {
    s: f32,
    cw: f32,
    ch: f32,
    pad: f32,
    /// Width of the minimap lane.
    lane: f32,
    /// Height of the title bar.
    bar: f32,
}

/// The values of the settings page. `App::theme` and `App::bottom` hold the theme in use and the prompt position.
#[derive(Clone, PartialEq)]
struct Settings {
    /// 0: One Dark, 1: One Light, 2: the theme of Windows.
    theme: u8,
    /// Colon popup. 0: auto, 1: always below, 2: always above.
    popup: u8,
    /// Terminal font name. Empty: the first installed font.
    font: String,
    /// Font size in logical pixels.
    font_px: f32,
    /// 0: beam, 1: block, 2: underline.
    cursor: u8,
    /// 0: tabs show with two or more tabs, 1: always, 2: never.
    tab_bar: u8,
    minimap: bool,
    /// Ctrl+Click opens a path or a link.
    links: bool,
    /// The caret and the popups move smoothly.
    animations: bool,
    /// Shell of the first tab. `None`: PowerShell.
    profile: Option<Profile>,
}

const THEMES: [&str; 3] = ["dark", "light", "system"];
const POPUPS: [&str; 3] = ["auto", "below", "above"];
const CURSORS: [&str; 3] = ["beam", "block", "underline"];
const TAB_BARS: [&str; 3] = ["auto", "always", "never"];

impl Settings {
    /// Reads `key=value` lines. A missing or wrong line keeps the default value.
    fn parse(text: &str) -> (Settings, bool) {
        let mut s = Settings {
            theme: 0,
            popup: 0,
            font: String::new(),
            font_px: FONT_PX,
            cursor: 0,
            tab_bar: 0,
            minimap: true,
            links: true,
            animations: true,
            profile: None,
        };
        let mut bottom = false;
        let pick = |list: &[&str], value: &str, default: u8| list.iter().position(|v| *v == value).map_or(default, |i| i as u8);
        for (key, value) in text.lines().filter_map(|line| line.split_once('=')) {
            let value = value.trim();
            match key.trim() {
                "theme" => s.theme = pick(&THEMES, value, s.theme),
                "position" => bottom = value == "bottom",
                "popup" => s.popup = pick(&POPUPS, value, s.popup),
                "font" => s.font = value.to_string(),
                "font_size" => s.font_px = value.parse().unwrap_or(FONT_PX).clamp(FONT_MIN, FONT_MAX),
                "cursor" => s.cursor = pick(&CURSORS, value, s.cursor),
                "tab_bar" => s.tab_bar = pick(&TAB_BARS, value, s.tab_bar),
                "minimap" => s.minimap = value != "off",
                "links" => s.links = value != "off",
                "animations" => s.animations = value != "off",
                "profile" => s.profile = Profile::ALL.into_iter().find(|p| format!("{p:?}") == value),
                _ => {},
            }
        }
        (s, bottom)
    }

    fn to_text(&self, bottom: bool) -> String {
        let on = |v: bool| if v { "on" } else { "off" };
        format!(
            "theme={}\nposition={}\npopup={}\nfont={}\nfont_size={}\ncursor={}\ntab_bar={}\nminimap={}\nlinks={}\nanimations={}\nprofile={}\n",
            THEMES[self.theme as usize],
            if bottom { "bottom" } else { "top" },
            POPUPS[self.popup as usize],
            self.font,
            self.font_px,
            CURSORS[self.cursor as usize],
            TAB_BARS[self.tab_bar as usize],
            on(self.minimap),
            on(self.links),
            on(self.animations),
            self.profile.map_or(String::new(), |p| format!("{p:?}")),
        )
    }

    fn theme(&self) -> Theme {
        match self.theme {
            1 => theme::ONE_LIGHT,
            2 if !system_dark() => theme::ONE_LIGHT,
            _ => theme::ONE_DARK,
        }
    }
}

/// Folder of `settings.txt`: `%APPDATA%\bebo`, or the folder in `BEBO_CONFIG`.
fn config_dir() -> Option<PathBuf> {
    std::env::var_os("BEBO_CONFIG").map(PathBuf::from).or_else(|| std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("bebo")))
}

/// `true` if Windows uses the dark mode for apps.
fn system_dark() -> bool {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .and_then(|key| key.get_value::<u32, _>("AppsUseLightTheme"))
        .map_or(true, |light| light == 0)
}

struct App {
    proxy: EventLoopProxy<UserEvent>,
    start: Instant,
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    gpu_rx: std::sync::mpsc::Receiver<render::Device>,
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
    drag_minimap: Option<usize>,
    last_click: (Instant, Point, u8),
    last_title_click: Instant,
    /// Time and tab of the last click on a tab. A double click renames the tab.
    last_tab_click: (Instant, usize),
    clipboard: Option<arboard::Clipboard>,
    profiles: Vec<Profile>,
    theme: Theme,
    /// Prompt position: the prompt stays at the bottom edge.
    bottom: bool,
    scale: f32,
    palette: Option<Palette>,
    colon: Option<Colon>,
    find: Option<Find>,
    menu: bool,
    /// Restart needs a confirm: (pane, clean).
    confirm: Option<(usize, bool)>,
    hits: Vec<(Rect, Hit)>,
    hover: Option<Hit>,
    quit: bool,
    settings: Settings,
    /// Page number of the settings tab.
    settings_page: usize,
    /// The font list is open. The value is the first row in view.
    font_menu: Option<f32>,
    anim: Anim,
    last_frame: Instant,
    /// A shell has new output that is not on the screen.
    dirty: bool,
    /// An animation needs the next frame.
    anim_due: bool,
    /// Time of the last write of the profile file.
    last_profile: Instant,
    /// Profiler: time of the last typed key, and the shell output after it came.
    key_at: Option<Instant>,
    echoed: bool,
    /// Time of the last key press of all types.
    last_input: Instant,
    /// The cell below the mouse: (pane, cell).
    hover_cell: Option<(usize, Point)>,
    /// The link below the mouse: (pane, grid line, first column, last column).
    hover_link: Option<(usize, i32, usize, usize)>,
    /// bebo runs as administrator.
    admin: bool,
    /// The icon of the window and of the taskbar button.
    icon: Option<Icon>,
}

impl App {
    fn focus(&self) -> usize {
        self.tabs[self.active].focus
    }

    /// The active tab is the settings tab.
    fn in_settings(&self) -> bool {
        self.tabs[self.active].settings
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

    fn metrics(&self) -> Metrics {
        let s = self.scale;
        let (cw, ch) = self.gpu.as_ref().map_or((8.0, 16.0), |g| (g.cell_w, g.cell_h));
        let lane = if self.settings.minimap { (14.0 * s).round() } else { 0.0 };
        Metrics { s, cw, ch, pad: (10.0 * s).round(), lane, bar: (30.0 * s).round() }
    }

    fn window_size(pane_cols: usize, pane_rows: usize, gpu: Option<&Gpu>) -> WindowSize {
        WindowSize {
            num_cols: pane_cols as u16,
            num_lines: pane_rows as u16,
            cell_width: gpu.map_or(8, |g| g.cell_w as u16),
            cell_height: gpu.map_or(16, |g| g.cell_h as u16),
        }
    }

    /// `size` is the expected size in cells. A correct first size prevents a redraw by the shell after the layout.
    fn new_pane(&mut self, cwd: Option<PathBuf>, profile: Profile, size: (usize, usize)) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        let (cols, rows) = (size.0.max(2), size.1.max(1));
        let proxy = Proxy { id, generation: 0, proxy: self.proxy.clone() };
        let term = pty::new_term(proxy.clone(), cols, rows);
        let size = Self::window_size(cols, rows, self.gpu.as_ref());
        let tx = pty::spawn_shell(term.clone(), proxy, size, cwd.clone(), profile, None).expect("cannot start the shell");
        let pane = Pane {
            term,
            tx,
            generation: 0,
            profile,
            cols,
            rows,
            input_start: None,
            await_input: false,
            cwd,
            title: profile.label().into(),
            blocks: Vec::new(),
            view_top: 0,
            cursor_px: None,
            badge: None,
            prompt_fix: None,
        };
        self.panes.insert(id, pane);
        id
    }

    fn focus_pane(&self) -> Option<&Pane> {
        self.tabs.get(self.active).and_then(|t| self.panes.get(&t.focus))
    }

    fn new_tab(&mut self, profile: Option<Profile>) {
        // A new tab starts in the home folder. The first tab starts in the folder of the bebo process.
        // A split keeps the folder of its pane: see `split`.
        let cwd = if self.tabs.is_empty() { None } else { std::env::var_os("USERPROFILE").map(PathBuf::from) };
        let profile = profile.or(self.focus_pane().map(|p| p.profile)).unwrap_or_else(Profile::default_profile);
        let size = self.focus_pane().map_or((100, 30), |p| (p.cols, p.rows));
        let id = self.new_pane(cwd, profile, size);
        self.tabs.push(Tab { root: Node::Leaf(id), focus: id, name: None, settings: false });
        self.active = self.tabs.len() - 1;
        self.relayout();
    }

    /// Splits the focused pane. If the pane is already in a split of this direction, the command closes the pane.
    fn split(&mut self, side_by_side: bool) {
        let focus = self.focus();
        if parent_split(&self.tabs[self.active].root, focus) == Some(side_by_side) {
            self.close_pane(focus);
            return;
        }
        let (cwd, profile, cols, rows) = self.focus_pane().map(|p| (p.cwd.clone(), p.profile, p.cols, p.rows)).unwrap();
        let size = if side_by_side { (cols / 2, rows) } else { (cols, rows / 2) };
        let id = self.new_pane(cwd, profile, size);
        let tab = &mut self.tabs[self.active];
        tab.root.split(focus, id, side_by_side);
        tab.focus = id;
        self.relayout();
    }

    fn remove_tab(&mut self, index: usize) {
        self.tabs.remove(index);
        // The settings tab alone does not keep bebo open.
        if self.tabs.iter().all(|t| t.settings) {
            self.quit = true;
            return;
        }
        // A tab on the left moves the active tab one place. If the active tab closed, the tab on its right becomes active.
        if self.active > index || self.active >= self.tabs.len() {
            self.active -= 1;
        }
        self.relayout();
    }

    /// Closes a pane. The id `usize::MAX` closes the settings tab.
    fn close_pane(&mut self, id: usize) {
        if id == usize::MAX {
            self.font_menu = None;
            if let Some(index) = self.tabs.iter().position(|t| t.settings) {
                self.remove_tab(index);
            }
            return;
        }
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
            None => return self.remove_tab(index),
        }
        self.relayout();
    }

    fn close_tab(&mut self, index: usize) {
        let Some(tab) = self.tabs.get(index) else { return };
        let mut leaves = Vec::new();
        tab.root.leaves(&mut leaves);
        for id in leaves {
            self.close_pane(id);
        }
    }

    /// Restarts now, or asks first if a command runs and `force` is off.
    fn request_restart(&mut self, id: usize, clean: bool, force: bool) {
        let running = self.panes.get(&id).and_then(|p| p.blocks.last()).is_some_and(|b| b.exec_at.is_some() && b.exit.is_none());
        if running && !force {
            self.confirm = Some((id, clean));
            self.redraw();
        } else {
            self.restart(id, clean);
        }
    }

    /// Stops the shell of the pane and starts a new shell with a new environment.
    /// `clean` removes the scrollback and starts in the home folder.
    fn restart(&mut self, id: usize, clean: bool) {
        let gpu = self.gpu.as_ref();
        let Some(pane) = self.panes.get_mut(&id) else { return };
        let _ = pane.tx.send(Msg::Shutdown);
        pane.generation += 1;
        pane.input_start = None;
        pane.await_input = false;
        // The content is also lost if the folder does not exist now.
        let lost = clean || !pane.cwd.as_ref().is_some_and(|p| p.is_dir());
        if lost {
            pane.cwd = std::env::var_os("USERPROFILE").map(PathBuf::from);
            pane.blocks.clear();
        }
        let line = "\u{2500}".repeat(6);
        let state = if lost { "content lost" } else { "content restored" };
        let banner = format!("{line} Session restarted \u{b7} {} \u{b7} {state} {line}", clock());
        let color = if lost { "33" } else { "90" };
        let powershell = matches!(pane.profile, Profile::Pwsh | Profile::PowerShell);
        {
            let mut term = pane.term.lock();
            let mut parser: Processor<StdSyncHandler> = Processor::new();
            // Leave the alternate screen and show the cursor. For a clean start, remove the scrollback also.
            let mut reset = String::from("\x1b[?1049l\x1b[0m\x1b[?25h");
            reset += if lost { "\x1b[H\x1b[2J\x1b[3J" } else { "\r\n" };
            if !powershell {
                reset += &format!("\x1b[{color}m{banner}\x1b[0m\r\n");
            }
            parser.advance(&mut *term, reset.as_bytes());
            term.selection = None;
            term.scroll_display(Scroll::Bottom);
        }
        pane.badge = Some(Badge {
            text: if lost { "content lost \u{b7} opened in home".into() } else { "content restored".into() },
            lost,
            until: (!lost).then(|| Instant::now() + Duration::from_secs(3)),
        });
        let proxy = Proxy { id, generation: pane.generation, proxy: self.proxy.clone() };
        let size = Self::window_size(pane.cols, pane.rows, gpu);
        let banner = powershell.then(|| format!("{color}|{banner}"));
        match pty::spawn_shell(pane.term.clone(), proxy, size, pane.cwd.clone(), pane.profile, banner) {
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
        self.scale = window.scale_factor() as f32;
        let mut gpu = Gpu::new(window, parts, self.settings.font_px * self.scale, self.start);
        gpu.set_mono(&self.settings.font);
        // Image ids: 0 is the bebo icon. 1 to 3 are the shell icons, see `Profile::image`.
        add_icon!(gpu, 0, "app/bebo");
        add_icon!(gpu, 1, "shell/powershell");
        add_icon!(gpu, 2, "shell/cmd");
        add_icon!(gpu, 3, "shell/wsl");
        self.gpu = Some(gpu);
        render::tick(self.start, "gpu ready");
        self.relayout();
        self.draw();

        // Test aid: BEBO_TEST runs these commands at start.
        // t: tab, d: split right, e: split down, f: find, p: palette, m: profile menu, l: light theme, b: bottom layout.
        for command in std::env::var("BEBO_TEST").unwrap_or_default().chars() {
            match command {
                't' => self.new_tab(None),
                'd' => self.split(true),
                'e' => self.split(false),
                'f' => {
                    self.find = Some(Find { query: "bebo".into(), ..Default::default() });
                    self.find_update();
                },
                'p' => self.palette = Some(Palette::default()),
                'm' => self.menu = true,
                'l' => self.theme = theme::ONE_LIGHT,
                's' => self.run(Cmd::Settings),
                'b' => self.bottom = true,
                _ => {},
            }
        }

        if std::env::var_os("BEBO_TIMING").is_some() {
            let gpu = self.gpu.as_ref().unwrap();
            render::tick(self.start, &format!("first frame, adapter: {}", gpu.adapter_name));
            let _ = std::fs::write(std::env::temp_dir().join("bebo-timing.txt"), &*render::TIMING.lock().unwrap());
        }
    }

    /// Calculates the pane rectangles and sends a new size to each shell that needs it.
    fn relayout(&mut self) {
        let m = self.metrics();
        let Some(gpu) = self.gpu.as_ref() else { return };
        let (width, height) = gpu.size();
        let area = Rect { x: 0.0, y: m.bar, w: width, h: (height - m.bar).max(1.0) };
        self.pane_rects.clear();
        self.dividers.clear();
        layout(&self.tabs[self.active].root, area, &mut Vec::new(), &mut self.pane_rects, &mut self.dividers);
        for (id, rect) in &self.pane_rects {
            let Some(pane) = self.panes.get_mut(id) else { continue };
            let cols = (((rect.w - m.pad * 1.5 - m.lane) / m.cw) as usize).max(2);
            let rows = (((rect.h - m.pad) / m.ch) as usize).max(1);
            if (cols, rows) != (pane.cols, pane.rows) {
                let same_width = cols == pane.cols && pane.prompt_fix.is_none_or(|(_, ok)| ok);
                pane.cols = cols;
                pane.rows = rows;
                // A new width moves the lines, so the block lines are not correct after a resize.
                pane.blocks.clear();
                resize_term(&mut pane.term.lock(), cols, rows);
                let _ = pane.tx.send(Msg::Resize(Self::window_size(cols, rows, Some(gpu))));
                // Wait until the resize stops. A window drag gives many sizes.
                pane.prompt_fix = Some((Instant::now() + Duration::from_millis(150), same_width));
            }
        }
        if let (Some(window), Some(pane)) = (&self.window, self.focus_pane()) {
            window.set_title(&format!("bebo \u{2013} {}", self.tab_title(self.active, pane)));
        }
        self.redraw();
    }

    fn tab_title(&self, index: usize, pane: &Pane) -> String {
        self.tabs[index].name.clone().unwrap_or_else(|| pane.title.clone())
    }

    fn pane_at(&self, x: f32, y: f32) -> Option<(usize, Rect)> {
        self.pane_rects.iter().copied().find(|(_, r)| r.contains(x, y))
    }

    fn pane_rect(&self, id: usize) -> Option<Rect> {
        self.pane_rects.iter().find(|(p, _)| *p == id).map(|(_, r)| *r)
    }

    fn cell_at(&self, id: usize, rect: Rect, x: f32, y: f32) -> Option<(Point, Side)> {
        let m = self.metrics();
        let pane = self.panes.get(&id)?;
        let col = ((x - rect.x - m.pad) / m.cw).max(0.0);
        let side = if col.fract() < 0.5 { Side::Left } else { Side::Right };
        let col = (col as usize).min(pane.cols - 1);
        let row = (((y - rect.y - m.pad * 0.6) / m.ch).max(0.0) as usize).min(pane.rows - 1);
        let first = -(pane.term.lock().grid().history_size() as i32);
        Some((Point::new(Line((pane.view_top + row as i32).max(first)), Column(col)), side))
    }

    fn mouse_report(&self, id: usize, button: u8, point: Point, pressed: bool) {
        let Some(pane) = self.panes.get(&id) else { return };
        let (col, row) = (point.column.0 + 1, point.line.0 + 1);
        let sgr = pane.term.lock().mode().contains(TermMode::SGR_MOUSE);
        let bytes = if sgr {
            format!("\x1b[<{button};{col};{row}{}", if pressed { 'M' } else { 'm' }).into_bytes()
        } else if col < 223 && (1..223).contains(&row) {
            let button = if pressed { button } else { 3 };
            vec![0x1b, b'[', b'M', 32 + button, 32 + col as u8, 32 + row as u8]
        } else {
            return;
        };
        self.send(id, bytes);
    }

    /// Direction of a window resize if the mouse is on the window edge.
    fn resize_edge(&self) -> Option<ResizeDirection> {
        let window = self.window.as_ref()?;
        if window.is_maximized() {
            return None;
        }
        let size = window.inner_size();
        let (x, y) = self.mouse;
        let e = 5.0 * self.scale;
        let (left, right) = (x < e, x > size.width as f32 - e);
        let (top, bottom) = (y < e, y > size.height as f32 - e);
        Some(match (left, right, top, bottom) {
            (true, _, true, _) => ResizeDirection::NorthWest,
            (_, true, true, _) => ResizeDirection::NorthEast,
            (true, _, _, true) => ResizeDirection::SouthWest,
            (_, true, _, true) => ResizeDirection::SouthEast,
            (true, ..) => ResizeDirection::West,
            (_, true, ..) => ResizeDirection::East,
            (_, _, true, _) => ResizeDirection::North,
            (_, _, _, true) => ResizeDirection::South,
            _ => return None,
        })
    }

    fn hit_at(&self, x: f32, y: f32) -> Option<Hit> {
        self.hits.iter().rev().find(|(r, _)| r.contains(x, y)).map(|(_, h)| *h)
    }

    fn mouse_press(&mut self) {
        let (x, y) = self.mouse;
        if let (Some(direction), Some(window)) = (self.resize_edge(), &self.window) {
            let _ = window.drag_resize_window(direction);
            return;
        }
        let hit = self.hit_at(x, y);
        // A click outside a popup closes the popup.
        if self.menu && !matches!(hit, Some(Hit::MenuItem(_) | Hit::Menu)) {
            self.menu = false;
            self.redraw();
        }
        if self.font_menu.is_some() && !matches!(hit, Some(Hit::Font(_) | Hit::Setting(3, _))) {
            self.font_menu = None;
            self.redraw();
        }
        if self.palette.is_some() && !matches!(hit, Some(Hit::PaletteItem(_) | Hit::Block)) {
            self.palette = None;
            self.redraw();
            return;
        }
        if let Some(hit) = hit {
            self.click(hit);
            return;
        }
        if let Some(i) = self.divider_at(x, y) {
            self.drag_divider = Some(i);
            return;
        }
        let Some((id, rect)) = self.pane_at(x, y) else { return };
        self.tabs[self.active].focus = id;
        let Some((point, side)) = self.cell_at(id, rect, x, y) else { return };
        if self.mods.control_key() && self.settings.links && self.open_link(id, point) {
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

    fn click(&mut self, hit: Hit) {
        let window = self.window.clone();
        match hit {
            Hit::TitleDrag => {
                let now = Instant::now();
                let double = now - self.last_title_click < Duration::from_millis(400);
                self.last_title_click = now;
                if let Some(window) = window {
                    if double {
                        window.set_maximized(!window.is_maximized());
                    } else {
                        let _ = window.drag_window();
                    }
                }
            },
            Hit::Tab(i) => {
                let now = Instant::now();
                let double = self.last_tab_click.1 == i && now - self.last_tab_click.0 < Duration::from_millis(400);
                self.last_tab_click = (now, i);
                self.active = i;
                // A double click opens the rename box with the current name.
                if double && !self.tabs[i].settings {
                    let query = self.tabs[i].name.clone().unwrap_or_default();
                    self.palette = Some(Palette { rename: true, query, ..Default::default() });
                }
                self.relayout();
            },
            // The second click of a double click on a narrow tab is on its close button. It is a rename, not a close.
            Hit::TabClose(i) if self.last_tab_click.1 == i && self.last_tab_click.0.elapsed() < Duration::from_millis(400) => {
                self.click(Hit::Tab(i));
            },
            Hit::TabClose(i) => self.close_tab(i),
            Hit::NewTab => self.new_tab(None),
            Hit::Menu => {
                self.menu = !self.menu;
                self.redraw();
            },
            Hit::MenuItem(i) => {
                self.menu = false;
                if let Some(profile) = self.profiles.get(i).copied() {
                    self.new_tab(Some(profile));
                } else {
                    self.run(Cmd::Settings);
                }
            },
            Hit::MenuAdmin(i) => {
                self.menu = false;
                if let Some(profile) = self.profiles.get(i).copied() {
                    self.open_admin(profile);
                }
                self.redraw();
            },
            Hit::Font(i) => self.set_font_family(i),
            Hit::WinMin => {
                if let Some(window) = window {
                    window.set_minimized(true);
                }
            },
            Hit::WinMax => {
                if let Some(window) = window {
                    window.set_maximized(!window.is_maximized());
                }
            },
            Hit::WinClose => self.quit = true,
            Hit::PaletteItem(i) => self.palette_run(i),
            Hit::ColonItem(i) => {
                if let Some(colon) = &mut self.colon {
                    colon.selected = i;
                }
                self.colon_run();
            },
            Hit::FindToggle(i) => {
                if let Some(find) = &mut self.find {
                    match i {
                        0 => find.case = !find.case,
                        1 => find.word = !find.word,
                        _ => find.regex = !find.regex,
                    }
                }
                self.find_update();
            },
            Hit::FindPrev => self.find_step(true),
            Hit::FindNext => self.find_step(false),
            Hit::FindClose => {
                self.find = None;
                self.redraw();
            },
            Hit::Minimap(id) => {
                self.drag_minimap = Some(id);
                self.minimap_scroll(id);
            },
            Hit::Setting(id, value) => self.set_setting(id, value),
            Hit::SettingsPage(page) => {
                self.settings_page = page;
                self.font_menu = None;
                self.redraw();
            },
            Hit::Block => {},
        }
    }

    /// Scrolls the pane so that the mouse position in the minimap is in the middle of the view.
    fn minimap_scroll(&mut self, id: usize) {
        let (Some(rect), Some(pane)) = (self.pane_rect(id), self.panes.get(&id)) else { return };
        let mut term = pane.term.lock();
        let history = term.grid().history_size() as i32;
        let total = (history + pane.rows as i32) as f32;
        let ratio = ((self.mouse.1 - rect.y) / rect.h).clamp(0.0, 1.0);
        let target = (ratio * total) as i32 - history - pane.rows as i32 / 2;
        term.scroll_display(Scroll::Delta(pane.view_top - target));
        drop(term);
        self.redraw();
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
        if let Some(id) = self.drag_minimap {
            self.minimap_scroll(id);
            return;
        }
        let hover = self.hit_at(x, y);
        if hover != self.hover {
            self.hover = hover;
            self.redraw();
        }
        // A path or a link below the mouse gets an underline.
        let cell = if hover.is_none() && self.settings.links && self.press.is_none() {
            self.pane_at(x, y).and_then(|(id, rect)| self.cell_at(id, rect, x, y).map(|(point, _)| (id, point)))
        } else {
            None
        };
        if cell != self.hover_cell {
            self.hover_cell = cell;
            // ponytail: each new cell asks the disk if the path exists. Add a cache if a network folder makes this slow.
            let link = cell.and_then(|(id, point)| self.link_at(id, point).map(|(_, first, last)| (id, point.line.0, first, last)));
            if link != self.hover_link {
                self.hover_link = link;
                self.redraw();
            }
        }
        if let Some(window) = &self.window {
            let icon = match (self.resize_edge(), hover, self.divider_at(x, y)) {
                (Some(ResizeDirection::East | ResizeDirection::West), ..) => CursorIcon::EwResize,
                (Some(ResizeDirection::North | ResizeDirection::South), ..) => CursorIcon::NsResize,
                (Some(ResizeDirection::NorthWest | ResizeDirection::SouthEast), ..) => CursorIcon::NwseResize,
                (Some(_), ..) => CursorIcon::NeswResize,
                (None, Some(_), _) => CursorIcon::Default,
                (None, None, Some(i)) if self.dividers[i].side_by_side => CursorIcon::ColResize,
                (None, None, Some(_)) => CursorIcon::RowResize,
                (None, None, None) if self.hover_link.is_some() && self.mods.control_key() => CursorIcon::Pointer,
                (None, None, None) => CursorIcon::Text,
            };
            window.set_cursor(icon);
        }
        let Some(press) = &self.press else { return };
        let (id, start, start_side, reported) = (press.pane, press.point, press.side, press.reported);
        let Some(rect) = self.pane_rect(id) else { return };
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
        if let Some(press) = &mut self.press {
            press.moved = true;
        }
        self.redraw();
    }

    fn mouse_release(&mut self) {
        self.drag_divider = None;
        self.drag_minimap = None;
        let Some(press) = self.press.take() else { return };
        if press.reported {
            let (x, y) = self.mouse;
            let point = self.pane_rect(press.pane).and_then(|r| self.cell_at(press.pane, r, x, y)).map_or(press.point, |(p, _)| p);
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
        if self.in_settings() {
            // The wheel moves the font list.
            let count = self.gpu.as_mut().map_or(0, |gpu| gpu.families().len());
            if let Some(scroll) = &mut self.font_menu {
                *scroll = (*scroll - lines as f32).clamp(0.0, count.saturating_sub(FONT_ROWS) as f32);
                self.redraw();
            }
            return;
        }
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
        let Some(text) = self.focus_pane().and_then(|p| p.term.lock().selection_to_string()) else { return };
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

    /// Finds all matches of the find text. The last match becomes the current match.
    fn find_update(&mut self) {
        let bottom = self.bottom;
        let (Some(find), Some(pane)) = (self.find.as_mut(), self.tabs.get(self.active).and_then(|t| self.panes.get(&t.focus)))
        else {
            return;
        };
        find.matches.clear();
        find.current = 0;
        if !find.query.is_empty() {
            let term = pane.term.lock();
            if let Ok(mut regex) = RegexSearch::new(&find.pattern()) {
                let start = Point::new(term.topmost_line(), Column(0));
                let end = Point::new(term.bottommost_line(), term.last_column());
                // ponytail: 2000 matches maximum. Add paging if a log needs more.
                let iter = RegexIter::new(start, end, Direction::Right, &term, &mut regex);
                find.matches = iter.take(2000).map(|m| (*m.start(), *m.end())).collect();
                find.current = find.matches.len().saturating_sub(1);
            }
            drop(term);
            Self::find_show(find, pane, bottom);
        }
        self.redraw();
    }

    /// Scrolls the current match into the view.
    fn find_show(find: &Find, pane: &Pane, bottom: bool) {
        let Some((start, _)) = find.matches.get(find.current) else { return };
        let mut term = pane.term.lock();
        let top = view_top(&term, pane.rows, bottom);
        if !(top..top + pane.rows as i32).contains(&start.line.0) {
            term.scroll_display(Scroll::Delta(top - (start.line.0 - pane.rows as i32 / 2)));
        }
    }

    fn find_step(&mut self, older: bool) {
        let bottom = self.bottom;
        let (Some(find), Some(pane)) = (self.find.as_mut(), self.tabs.get(self.active).and_then(|t| self.panes.get(&t.focus)))
        else {
            return;
        };
        let n = find.matches.len();
        if n > 0 {
            find.current = if older { (find.current + n - 1) % n } else { (find.current + 1) % n };
            Self::find_show(find, pane, bottom);
        }
        self.redraw();
    }

    /// Scrolls to the prompt above or below the top line of the view.
    fn jump_prompt(&mut self, up: bool) {
        let Some(pane) = self.focus_pane() else { return };
        let mut term = pane.term.lock();
        let history = term.grid().history_size() as i32;
        let top = pane.view_top;
        let lines = pane.blocks.iter().map(|b| b.line - history);
        let target = if up { lines.filter(|l| *l < top).max() } else { lines.filter(|l| *l > top).min() };
        match target {
            Some(line) => term.scroll_display(Scroll::Delta(top - line)),
            None if !up => term.scroll_display(Scroll::Bottom),
            None => {},
        }
        drop(term);
        self.redraw();
    }

    /// The URL or the file path at `point`: the target to open, and the first and last column of its text.
    fn link_at(&self, id: usize, point: Point) -> Option<(String, usize, usize)> {
        let pane = self.panes.get(&id)?;
        let term = pane.term.lock();
        let grid = term.grid();
        let (text, first, last) = match grid[point.line][point.column].hyperlink() {
            Some(link) => (link.uri().to_string(), point.column.0, point.column.0),
            None => {
                let (mut chars, mut columns) = (Vec::new(), Vec::new());
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
                    columns.push(col);
                }
                let (start, end) = token_range(&chars, index)?;
                (chars[start..end].iter().collect(), columns[start], columns[end - 1])
            },
        };
        drop(term);
        resolve_link(&text, pane.cwd.as_deref()).map(|target| (target, first, last))
    }

    /// Opens the URL or the file path at `point`. Returns `false` if there is no link.
    fn open_link(&self, id: usize, point: Point) -> bool {
        let Some((target, ..)) = self.link_at(id, point) else { return false };
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

    /// `font_px` is in physical pixels.
    fn set_font(&mut self, font_px: f32) {
        let font_px = (font_px / self.scale).round().clamp(FONT_MIN, FONT_MAX) * self.scale;
        if let Some(gpu) = self.gpu.as_mut() {
            gpu.set_font_px(font_px);
            self.settings.font_px = (gpu.font_px / self.scale).round();
        }
        // Ctrl+= and Ctrl+- change the same setting as the settings page.
        self.save_state();
        // Force a new size for each pane.
        for pane in self.panes.values_mut() {
            pane.cols = 0;
        }
        self.relayout();
    }

    /// The list of the palette.
    fn entries(&self) -> Vec<Entry> {
        let entry = |category, title: &str, keys, cmd| Entry { category, title: title.to_string(), keys, cmd, current: false };
        let mut list = vec![
            entry("Tab", "New tab", "Ctrl+Shift+T", Cmd::NewTab),
            entry("Tab", "Rename tab...", "", Cmd::RenameTab),
            entry("Pane", "Split top and bottom", "Alt+Shift+-", Cmd::SplitDown),
            entry("Pane", "Split left and right", "Alt+Shift++", Cmd::SplitRight),
            entry("Find", "Find in output", "Ctrl+Shift+F", Cmd::Find),
            entry("Session", "Restart shell \u{b7} restore content", "Ctrl+Shift+R", Cmd::Restart),
            entry("Session", "Restart shell \u{b7} force", "", Cmd::RestartForce),
            entry("Session", "Restart shell \u{b7} clean, content lost", "", Cmd::RestartClean),
            entry("Session", "Clear output", "Ctrl+K", Cmd::Clear),
            entry("Pane", "Close pane", "Ctrl+Shift+W", Cmd::ClosePane),
            entry("Tab", "Next tab", "Ctrl+Tab", Cmd::NextTab),
            entry("Edit", "Copy", "Ctrl+Shift+C", Cmd::Copy),
            entry("Edit", "Paste", "Ctrl+Shift+V", Cmd::Paste),
            entry("View", "Settings", "Ctrl+,", Cmd::Settings),
        ];
        for dark in [true, false] {
            let name = if dark { "Theme \u{b7} One Dark" } else { "Theme \u{b7} One Light" };
            list.push(Entry { current: self.theme.dark == dark, ..entry("View", name, "", Cmd::Theme(dark)) });
        }
        let position = if self.bottom { "Prompt position \u{b7} top" } else { "Prompt position \u{b7} bottom" };
        list.push(entry("View", position, "", Cmd::Layout));
        for profile in &self.profiles {
            list.push(entry("Profile", &format!("Open {}", profile.name()), "", Cmd::Profile(*profile)));
        }
        for profile in &self.profiles {
            list.push(entry("Admin", &format!("Open {} as administrator", profile.name()), "", Cmd::Admin(*profile)));
        }
        list
    }

    /// The palette rows that match the typed text, with the positions of the matched characters.
    fn palette_rows(&self) -> Vec<(Entry, Vec<usize>)> {
        let query = self.palette.as_ref().map_or("", |p| p.query.as_str());
        let mut rows: Vec<(Entry, Vec<usize>)> =
            self.entries().into_iter().filter_map(|e| fuzzy(&e.title, query).map(|positions| (e, positions))).collect();
        // The best match has its characters near the start and near each other.
        rows.sort_by_key(|(_, p)| p.first().map_or(0, |first| first * 4 + (p.last().unwrap() - first)));
        rows
    }

    fn palette_run(&mut self, index: usize) {
        let Some(palette) = self.palette.take() else { return };
        if palette.rename {
            let name = palette.query.trim();
            self.tabs[self.active].name = (!name.is_empty()).then(|| name.to_string());
            self.relayout();
            return;
        }
        self.palette = Some(palette);
        let cmd = self.palette_rows().get(index).map(|(e, _)| e.cmd);
        self.palette = None;
        match cmd {
            Some(cmd) => self.run(cmd),
            None => self.redraw(),
        }
    }

    fn run(&mut self, cmd: Cmd) {
        let focus = self.focus();
        // The settings tab has no pane. Only the commands for tabs and for the window apply there.
        let general = matches!(
            cmd,
            Cmd::NewTab | Cmd::NextTab | Cmd::ClosePane | Cmd::Layout | Cmd::Theme(_) | Cmd::Palette | Cmd::Profile(_) | Cmd::Admin(_) | Cmd::Settings
        );
        if self.in_settings() && !general {
            return;
        }
        match cmd {
            Cmd::NewTab => self.new_tab(None),
            Cmd::RenameTab => {
                self.menu = false;
                self.palette = Some(Palette { rename: true, ..Default::default() });
            },
            Cmd::NextTab => {
                self.active = (self.active + 1) % self.tabs.len();
                self.relayout();
            },
            Cmd::SplitDown => self.split(false),
            Cmd::SplitRight => self.split(true),
            Cmd::ClosePane => self.close_pane(focus),
            Cmd::Find => {
                if self.find.is_none() {
                    self.find = Some(Find::default());
                }
            },
            Cmd::Restart => self.request_restart(focus, false, false),
            Cmd::RestartForce => self.request_restart(focus, false, true),
            Cmd::RestartClean => self.request_restart(focus, true, false),
            Cmd::Clear => {
                if let Some(pane) = self.panes.get_mut(&focus) {
                    let mut parser: Processor<StdSyncHandler> = Processor::new();
                    parser.advance(&mut *pane.term.lock(), b"\x1b[H\x1b[2J\x1b[3J");
                    pane.blocks.clear();
                    // Ctrl+L makes the shell draw the prompt again.
                    let _ = pane.tx.send(Msg::Input(Cow::Borrowed(b"\x0c")));
                }
            },
            Cmd::Layout => {
                self.bottom = !self.bottom;
                self.save_state();
            },
            Cmd::Theme(dark) => {
                self.settings.theme = if dark { 0 } else { 1 };
                self.theme = self.settings.theme();
                self.save_state();
            },
            // One popup at a time: the palette closes the profile menu.
            Cmd::Palette => {
                self.menu = false;
                self.palette = Some(Palette::default());
            },
            Cmd::Profile(profile) => self.new_tab(Some(profile)),
            Cmd::Admin(profile) => self.open_admin(profile),
            Cmd::Copy => self.copy(),
            Cmd::Paste => self.paste(),
            Cmd::Settings => {
                match self.tabs.iter().position(|t| t.settings) {
                    Some(index) => self.active = index,
                    None => {
                        self.tabs.push(Tab { root: Node::Leaf(usize::MAX), focus: usize::MAX, name: None, settings: true });
                        self.active = self.tabs.len() - 1;
                    },
                }
                self.relayout();
            },
        }
        self.redraw();
    }

    /// Runs the selected row of the colon popup.
    fn colon_run(&mut self) {
        let Some(colon) = self.colon.take() else { return };
        let focus = self.focus();
        let items = colon_items(&colon.text);
        let selected = items.get(colon.selected).map(|(name, ..)| name.trim_start_matches(':').to_string());
        // `:rename name` gives the name to the tab. `:rename` alone opens the rename box.
        if let Some(name) = colon.text.strip_prefix("rename ") {
            let name = name.trim();
            self.tabs[self.active].name = (!name.is_empty()).then(|| name.to_string());
            self.relayout();
            return;
        }
        if colon.text == "restart" || colon.text.starts_with("restart ") {
            let mut words: Vec<&str> = colon.text.split_whitespace().skip(1).collect();
            // A part of a word selects the row of the popup.
            if let (Some(last), Some(selected)) = (words.last_mut(), selected.as_deref())
                && !RESTART_ARGS.iter().any(|(n, _)| n == last)
            {
                *last = selected;
            }
            self.request_restart(focus, words.contains(&"clean"), words.contains(&"force"));
            return;
        }
        match selected.as_deref() {
            Some("theme") => self.palette = Some(Palette { query: "theme".into(), ..Default::default() }),
            Some("profile") => self.palette = Some(Palette { query: "open".into(), ..Default::default() }),
            Some(name) => {
                if let Some((.., cmd)) = COLON.iter().find(|(n, ..)| *n == name) {
                    self.run(*cmd);
                }
            },
            // No such command: the shell gets the text.
            None => self.send(focus, format!(":{}\r", colon.text).into_bytes()),
        }
        self.redraw();
    }

    /// Keys for the colon popup. Returns `true` if the popup used the key.
    fn colon_key(&mut self, key: &Key, text: Option<&str>) -> bool {
        let focus = self.focus();
        let plain = !self.mods.control_key() && !self.mods.alt_key();
        let Some(colon) = self.colon.as_mut() else {
            // A colon at the start of an empty command line opens the popup.
            let empty = self.focus_pane().is_some_and(|pane| {
                Region::new(pane, &pane.term.lock()).is_some_and(|r| r.cursor == 0 && r.end == 0)
            });
            if plain && text == Some(":") && empty {
                self.colon = Some(Colon::default());
                self.redraw();
                return true;
            }
            return false;
        };
        let count = colon_items(&colon.text).len().max(1);
        match key {
            Key::Named(NamedKey::Escape) => {
                // Esc closes the popup and keeps the text on the command line.
                let text = format!(":{}", colon.text);
                self.colon = None;
                self.send(focus, text.into_bytes());
            },
            Key::Named(NamedKey::Enter) => self.colon_run(),
            Key::Named(NamedKey::ArrowDown) => colon.selected = (colon.selected + 1) % count,
            Key::Named(NamedKey::ArrowUp) => colon.selected = (colon.selected + count - 1) % count,
            // Tab completes a command name. It does not change the name after `rename`.
            Key::Named(NamedKey::Tab) if colon.text.starts_with("rename ") => {},
            Key::Named(NamedKey::Tab) => {
                if let Some((name, ..)) = colon_items(&colon.text).get(colon.selected) {
                    let name = name.trim_start_matches(':');
                    if colon.text.starts_with("restart") && name != "restart" {
                        let keep = colon.text.rfind(' ').map_or("restart ".len().min(colon.text.len()), |i| i + 1);
                        colon.text.truncate(keep);
                        if !colon.text.ends_with(' ') {
                            colon.text.push(' ');
                        }
                        colon.text += name;
                    } else {
                        colon.text = name.to_string();
                    }
                    colon.text.push(' ');
                    colon.selected = 0;
                }
            },
            Key::Named(NamedKey::Backspace) => {
                if colon.text.pop().is_none() {
                    self.colon = None;
                }
            },
            Key::Named(NamedKey::Space) if colon.text.is_empty() => {
                // A colon and a space go to the shell.
                self.colon = None;
                self.send(focus, b": ".to_vec());
            },
            _ => {
                if let (Some(text), true) = (text, plain)
                    && !text.chars().any(char::is_control)
                {
                    colon.text.push_str(text);
                    colon.selected = 0;
                }
            },
        }
        self.redraw();
        true
    }

    fn key(&mut self, event: KeyEvent) {
        if event.state != ElementState::Pressed {
            return;
        }
        self.last_input = Instant::now();
        // Test aid: the test tool cannot hold a key. With BEBO_TEST, F9 changes the Ctrl state and F10 changes the Shift state.
        if let Key::Named(named @ (NamedKey::F9 | NamedKey::F10)) = &event.logical_key
            && std::env::var_os("BEBO_TEST").is_some()
        {
            self.mods.toggle(if *named == NamedKey::F9 { ModifiersState::CONTROL } else { ModifiersState::SHIFT });
            return;
        }
        let (ctrl, shift, alt) = (self.mods.control_key(), self.mods.shift_key(), self.mods.alt_key());
        let key = event.logical_key.clone();
        let text = event.text.as_deref().filter(|t| !t.chars().any(char::is_control));
        let focus = self.focus();

        // The first key removes the restart message.
        if let Some(pane) = self.panes.get_mut(&focus) {
            pane.badge = None;
        }

        if let Some(palette) = self.palette.as_mut() {
            let rename = palette.rename;
            match &key {
                Key::Named(NamedKey::Escape) => self.palette = None,
                Key::Named(NamedKey::Enter) => {
                    let selected = palette.selected;
                    self.palette_run(selected);
                },
                Key::Named(NamedKey::ArrowDown) => palette.selected += 1,
                Key::Named(NamedKey::ArrowUp) => palette.selected = palette.selected.saturating_sub(1),
                Key::Named(NamedKey::Backspace) => {
                    palette.query.pop();
                    palette.selected = 0;
                },
                _ => {
                    if let (Some(text), false) = (text, ctrl) {
                        palette.query.push_str(text);
                        palette.selected = 0;
                    }
                },
            }
            if !rename {
                let count = self.palette_rows().len();
                if let Some(palette) = self.palette.as_mut() {
                    palette.selected = palette.selected.min(count.saturating_sub(1));
                }
            }
            self.redraw();
            return;
        }

        // The settings tab uses the mouse. Esc closes the font list, then the tab.
        let in_settings = self.in_settings();
        if in_settings && key == Key::Named(NamedKey::Escape) {
            if self.font_menu.take().is_none() {
                self.close_pane(usize::MAX);
            }
            self.redraw();
            return;
        }

        if let Some((id, clean)) = self.confirm {
            match &key {
                Key::Named(NamedKey::Enter) => {
                    self.confirm = None;
                    self.restart(id, clean);
                },
                Key::Named(NamedKey::Escape) => self.confirm = None,
                _ => {},
            }
            self.redraw();
            return;
        }

        if self.menu {
            self.menu = false;
            self.redraw();
            if key == Key::Named(NamedKey::Escape) {
                return;
            }
        }

        if !in_settings && let Some(find) = self.find.as_mut() {
            let mut used = true;
            match (&key, alt) {
                (Key::Named(NamedKey::Escape), _) => {
                    self.find = None;
                    self.redraw();
                },
                // Enter goes to the older match, as in most terminals. Shift+Enter goes to the newer match.
                (Key::Named(NamedKey::Enter), _) => self.find_step(!shift),
                (Key::Named(NamedKey::Backspace), _) => {
                    find.query.pop();
                    self.find_update();
                },
                (Key::Character(c), true) if matches!(c.to_lowercase().as_str(), "c" | "w" | "r") => {
                    match c.to_lowercase().as_str() {
                        "c" => find.case = !find.case,
                        "w" => find.word = !find.word,
                        _ => find.regex = !find.regex,
                    }
                    self.find_update();
                },
                _ => match (text, ctrl || alt) {
                    (Some(text), false) => {
                        find.query.push_str(text);
                        self.find_update();
                    },
                    _ => used = false,
                },
            }
            if used {
                return;
            }
        }

        let character = match &key {
            Key::Character(c) => c.to_lowercase(),
            _ => String::new(),
        };
        let code = match event.physical_key {
            PhysicalKey::Code(code) => Some(code),
            _ => None,
        };
        let has_selection = self.focus_pane().is_some_and(|p| p.term.lock().selection.as_ref().is_some_and(|s| !s.is_empty()));
        let font_px = self.gpu.as_ref().map_or(FONT_PX, |g| g.font_px);
        // A full-screen program (nano, vim) gets Ctrl+K.
        let full_screen = self.focus_pane().is_some_and(|p| p.term.lock().mode().contains(TermMode::ALT_SCREEN));

        // bebo commands.
        match (ctrl, shift, alt, character.as_str(), &key) {
            (true, true, false, "t", _) => self.run(Cmd::NewTab),
            (true, true, false, "w", _) => self.run(Cmd::ClosePane),
            (true, true, false, "r", _) => self.run(Cmd::Restart),
            (true, true, false, "f", _) => self.run(Cmd::Find),
            (true, true, false, "p", _) => self.run(Cmd::Palette),
            (true, true, false, "c", _) => self.copy(),
            (true, _, false, "v", _) => self.paste(),
            (false, true, true, ..) if code == Some(KeyCode::Minus) => self.run(Cmd::SplitDown),
            (false, true, true, ..) if code == Some(KeyCode::Equal) => self.run(Cmd::SplitRight),
            (true, true, false, ..) if matches!(code, Some(KeyCode::Digit1 | KeyCode::Digit2 | KeyCode::Digit3 | KeyCode::Digit4)) => {
                let index = [KeyCode::Digit1, KeyCode::Digit2, KeyCode::Digit3, KeyCode::Digit4].iter().position(|c| Some(*c) == code);
                if let Some(profile) = index.and_then(|i| self.profiles.get(i)).copied() {
                    self.new_tab(Some(profile));
                }
            },
            (true, false, false, "c", _) if has_selection => {
                self.copy();
                if let Some(pane) = self.focus_pane() {
                    pane.term.lock().selection = None;
                }
                self.redraw();
            },
            (true, false, false, "=" | "+", _) => self.set_font(font_px + 1.0),
            (true, false, false, "-", _) => self.set_font(font_px - 1.0),
            (true, false, false, "0", _) => self.set_font(FONT_PX * self.scale),
            (true, false, false, ",", _) => self.run(Cmd::Settings),
            (true, false, false, "k", _) if !full_screen => self.run(Cmd::Clear),
            (true, _, false, _, Key::Named(NamedKey::Tab)) => {
                let n = self.tabs.len();
                self.active = if shift { (self.active + n - 1) % n } else { (self.active + 1) % n };
                self.relayout();
            },
            (false, true, false, _, Key::Named(NamedKey::PageUp | NamedKey::PageDown)) => {
                let scroll = if key == Key::Named(NamedKey::PageUp) { Scroll::PageUp } else { Scroll::PageDown };
                if let Some(pane) = self.focus_pane() {
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
            _ if in_settings => {},
            _ => {
                if !self.colon_key(&key, event.text.as_deref()) {
                    self.type_key(&key, event.text.as_deref());
                }
            },
        }
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
        let region = Region::new(pane, &term);
        let edit = if is_text || is_delete { region.as_ref().and_then(|r| r.delete_selection(&term)) } else { None };
        // Keep the command text for the restart confirm.
        let command = match (&region, key) {
            (Some(r), Key::Named(NamedKey::Enter)) => Some(term.bounds_to_string(r.point(0), r.point((r.end - 1).max(0)))),
            _ => None,
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
        // A frame is necessary only if the key changes the view. Else the echo of the shell draws the next frame.
        // One frame less for each key: the frame waits for the screen refresh and delays the echo frame.
        let view_changes = term.selection.is_some() || term.grid().display_offset() != 0;
        term.selection = None;
        term.scroll_display(Scroll::Bottom);
        drop(term);
        if matches!(key, Key::Named(NamedKey::Enter)) {
            pane.input_start = None;
            if let (Some(command), Some(block)) = (command, pane.blocks.last_mut()) {
                block.command = command.trim().to_string();
            }
        }
        let _ = pane.tx.send(Msg::Input(Cow::Owned(out)));
        self.key_at = Some(Instant::now());
        self.echoed = false;
        if view_changes {
            self.redraw();
        }
    }

    fn term_event(&mut self, id: usize, generation: u32, event: Event) {
        // New output can move the text away from the link underline.
        if matches!(event, Event::Wakeup) && self.hover_link.is_some_and(|link| link.0 == id) {
            self.hover_link = None;
            self.hover_cell = None;
        }
        let theme = self.theme;
        let gpu = self.gpu.as_ref();
        let Some(pane) = self.panes.get_mut(&id) else { return };
        match event {
            Event::Wakeup => {
                render::prof::count("term: wake-ups", 1);
                if let (Some(key_at), false) = (self.key_at, self.echoed) {
                    render::prof::add("latency: key to output", key_at);
                }
                self.echoed = self.key_at.is_some();
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
                    pane.blocks.push(Block { line, command: String::new(), exec_at: None, exit: None, duration: None });
                }
                // `about_to_wait` draws the output, with a limit on the number of frames in a second.
                self.dirty = true;
            },
            // A shell sets its program path as the title. The tab then keeps the name of the profile.
            Event::Title(title) if title.to_ascii_lowercase().contains(pane.profile.program()) => {},
            Event::Title(title) => {
                pane.title = title;
                if id == self.focus() {
                    self.relayout();
                }
            },
            Event::PtyWrite(text) => self.send(id, text.into_bytes()),
            Event::ColorRequest(index, format) => {
                let color = pane.term.lock().colors()[index];
                let [r, g, b] = color.map_or(theme.palette(index), |c| [c.r, c.g, c.b]);
                let text = format(alacritty_terminal::vte::ansi::Rgb { r, g, b });
                self.send(id, text.into_bytes());
            },
            Event::TextAreaSizeRequest(format) => {
                let text = format(Self::window_size(pane.cols, pane.rows, gpu));
                self.send(id, text.into_bytes());
            },
            Event::ClipboardStore(_, text) => self.set_clipboard(text),
            Event::ChildExit(_) | Event::Exit if generation == pane.generation => self.close_pane(id),
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
                pane.cwd = Some(PathBuf::from(path));
                self.relayout();
            },
            Mark::Restart => self.request_restart(id, false, false),
        }
    }

    /// Changes one setting from the settings page. `id` is the row and `value` is the choice.
    fn set_setting(&mut self, id: u8, value: u8) {
        match id {
            0 => {
                self.settings.theme = value.min(2);
                self.theme = self.settings.theme();
            },
            1 => self.bottom = value == 1,
            2 => self.settings.popup = value.min(2),
            3 => {
                // Opens or closes the font list. The list opens near the font in use.
                self.font_menu = match (self.font_menu, self.gpu.as_mut()) {
                    (None, Some(gpu)) => {
                        let current = gpu.mono().to_string();
                        let families = gpu.families();
                        let index = families.iter().position(|(name, _)| *name == current).unwrap_or(0);
                        Some(index.saturating_sub(3).min(families.len().saturating_sub(FONT_ROWS)) as f32)
                    },
                    _ => None,
                };
                self.redraw();
                return;
            },
            4 => {
                let px = self.settings.font_px + if value == 1 { 1.0 } else { -1.0 };
                self.set_font(px * self.scale);
            },
            5 => self.settings.cursor = value.min(2),
            6 => self.settings.tab_bar = value.min(2),
            7 => self.settings.minimap = !self.settings.minimap,
            8 => self.settings.links = !self.settings.links,
            9 => self.settings.animations = !self.settings.animations,
            10 => self.settings.profile = self.profiles.get(value as usize).copied(),
            _ => {},
        }
        self.apply_settings();
    }

    /// Selects row `index` of the font list as the terminal font.
    fn set_font_family(&mut self, index: usize) {
        self.font_menu = None;
        if let Some(gpu) = self.gpu.as_mut()
            && let Some((name, _)) = gpu.families().get(index).cloned()
        {
            gpu.set_mono(&name);
            self.settings.font = name;
        }
        self.apply_settings();
    }

    fn apply_settings(&mut self) {
        self.save_state();
        // The font and the minimap change the number of columns.
        for pane in self.panes.values_mut() {
            pane.cols = 0;
        }
        self.relayout();
    }

    /// Starts a new bebo window as administrator with this shell. Windows asks the user first.
    fn open_admin(&self, profile: Profile) {
        use std::os::windows::ffi::OsStrExt;
        let wide = |text: &std::ffi::OsStr| text.encode_wide().chain(Some(0)).collect::<Vec<u16>>();
        let Ok(exe) = std::env::current_exe() else { return };
        // `\.` at the end keeps a last backslash of the folder away from the quote.
        let folder = self.focus_pane().and_then(|p| p.cwd.as_ref()).map_or(String::new(), |d| format!(" --cwd \"{}\\.\"", d.display()));
        let (verb, exe) = (wide("runas".as_ref()), wide(exe.as_os_str()));
        let arguments = wide(format!("--profile {profile:?}{folder}").as_ref());
        // SAFETY: each string ends with a zero and lives until the call returns.
        unsafe {
            windows_sys::Win32::UI::Shell::ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), exe.as_ptr(), arguments.as_ptr(), std::ptr::null(), 1);
        }
    }

    /// Writes the settings to a file. bebo reads the file at the next start.
    fn save_state(&self) {
        let Some(dir) = config_dir() else { return };
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join("settings.txt"), self.settings.to_text(self.bottom));
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // bebo draws its own title bar. The shadow keeps the border and the rounded corners of Windows 11.
        let attributes = Window::default_attributes()
            .with_title("bebo")
            .with_inner_size(LogicalSize::new(1000.0, 610.0))
            // Below this size, the title bar and the popups have no room.
            .with_min_inner_size(LogicalSize::new(420.0, 240.0))
            .with_decorations(false)
            // A test window does not take the keyboard focus from the user.
            .with_active(std::env::var_os("BEBO_TEST").is_none())
            .with_window_icon(self.icon.clone())
            .with_taskbar_icon(self.icon.clone())
            .with_undecorated_shadow(true);
        let window = Arc::new(event_loop.create_window(attributes).expect("create window"));
        // The window shows immediately with the background color. The GPU frame comes when the device is ready.
        paint_background(&window, self.theme.bg);
        self.window = Some(window);
        render::tick(self.start, "window");
        self.start_gpu();
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Term(id, generation, event) => self.term_event(id, generation, event),
            UserEvent::Mark(id, mark) => self.mark(id, mark),
            UserEvent::GpuReady => self.start_gpu(),
        }
        if self.quit {
            event_loop.exit();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            // A minimized window has the size zero. Keep the layout, so that the shells keep their size.
            WindowEvent::Resized(size) if size.width == 0 || size.height == 0 => {},
            WindowEvent::Resized(size) => {
                if let Some(gpu) = self.gpu.as_mut() {
                    gpu.resize(size.width, size.height);
                }
                self.relayout();
            },
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale = scale_factor as f32;
                self.set_font(self.settings.font_px * self.scale);
            },
            WindowEvent::RedrawRequested if self.gpu.is_none() => {
                if let Some(window) = &self.window {
                    paint_background(window, self.theme.bg);
                }
            },
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::ModifiersChanged(mods) => self.mods = mods.state(),
            WindowEvent::KeyboardInput { event, .. } => self.key(event),
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse = (position.x as f32, position.y as f32);
                self.mouse_move();
            },
            // The test tool posts mouse messages while the real mouse is in a different window. Ignore the event then.
            WindowEvent::CursorLeft { .. } if std::env::var_os("BEBO_TEST").is_none() => {
                self.mouse = (-1.0, -1.0);
                self.hover = None;
                self.hover_cell = None;
                self.hover_link = None;
                self.redraw();
            },
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => match state {
                ElementState::Pressed => self.mouse_press(),
                ElementState::Released => self.mouse_release(),
            },
            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => (y * 3.0).round() as i32,
                    MouseScrollDelta::PixelDelta(p) => (p.y as f32 / self.metrics().ch) as i32,
                };
                if lines != 0 {
                    self.wheel(lines);
                }
            },
            WindowEvent::Focused(_) => self.redraw(),
            _ => {},
        }
        if self.quit {
            event_loop.exit();
        }
    }

    /// Wakes up when a restart message must go away, or when a prompt needs a redraw after a resize.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        if std::env::var_os("BEBO_PROFILE").is_some() && now - self.last_profile > Duration::from_secs(1) {
            self.last_profile = now;
            render::prof::write();
        }
        let mut next: Option<Instant> = None;
        let mut changed = false;
        for pane in self.panes.values_mut() {
            match pane.prompt_fix {
                Some((time, ok)) if time <= now => {
                    pane.prompt_fix = None;
                    // Ctrl+F12 runs the key handler of the bebo shell script. Only a PowerShell prompt gets the key.
                    if ok && pane.input_start.is_some() && matches!(pane.profile, Profile::Pwsh | Profile::PowerShell) {
                        let _ = pane.tx.send(Msg::Input(Cow::Borrowed(b"[24;5~")));
                    }
                },
                Some((time, _)) => next = Some(next.map_or(time, |n| n.min(time))),
                None => {},
            }
            match pane.badge.as_ref().and_then(|b| b.until) {
                Some(until) if until <= now => {
                    pane.badge = None;
                    changed = true;
                },
                Some(until) => next = Some(next.map_or(until, |n| n.min(until))),
                None => {},
            }
        }
        if changed {
            self.redraw();
        }
        // Fast output gives thousands of wake-ups in a second. One frame for each 16 ms is sufficient.
        // Animation frames: one for each refresh of the display at most.
        if self.anim_due {
            let hz = self.window.as_ref().and_then(|w| w.current_monitor()).and_then(|m| m.refresh_rate_millihertz()).unwrap_or(60_000);
            let due = self.last_frame + Duration::from_micros(1_000_000_000 / hz.max(30_000) as u64);
            if due <= now {
                self.anim_due = false;
                self.redraw();
            } else {
                next = Some(next.map_or(due, |n| n.min(due)));
            }
        }
        if self.dirty {
            // After a key press, the echo of the shell shows immediately. The limit is for output that the user did not type.
            let typing = now - self.last_input < Duration::from_millis(100);
            let due = self.last_frame + Duration::from_millis(16);
            if due <= now || typing {
                self.dirty = false;
                self.redraw();
            } else {
                next = Some(next.map_or(due, |n| n.min(due)));
            }
        }
        event_loop.set_control_flow(next.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
    }
}

/// Changes the size of the grid. When the pane grows, alacritty_terminal moves scrollback lines down into the screen.
/// ConPTY does not: it keeps the top line and draws its screen again, on top of those lines.
/// This function moves the lines back to the scrollback, or the shell output erases them.
/// When the width also changes, ConPTY moves lines into the screen as alacritty_terminal does. Then the lines stay.
/// ponytail: not done while a full-screen program runs, because the main grid is not accessible then.
fn resize_term<T: alacritty_terminal::event::EventListener>(term: &mut Term<T>, cols: usize, rows: usize) {
    let old_rows = term.screen_lines();
    let same_width = term.columns() == cols;
    let full_screen = term.mode().contains(TermMode::ALT_SCREEN);
    term.resize(TermSize { cols, rows });
    let grid = term.grid_mut();
    // The cursor row is the limit: a cursor above row 0 stops the PTY reader thread with an index error.
    let pulled = rows.saturating_sub(old_rows).min(grid.cursor.point.line.0.max(0) as usize);
    if pulled > 0 && same_width && !full_screen {
        grid.scroll_up(&(Line(0)..Line(rows as i32)), pulled);
        grid.cursor.point.line.0 -= pulled as i32;
        let saved = &mut grid.saved_cursor.point.line.0;
        *saved = (*saved - pulled as i32).max(0);
    }
}

/// Grid line at the top row of the view.
/// With the prompt at the bottom, blank rows below the cursor move out and scrollback lines come in at the top.
fn view_top(term: &Term<Proxy>, rows: usize, bottom: bool) -> i32 {
    let grid = term.grid();
    let mut shift = 0;
    if bottom && !term.mode().contains(TermMode::ALT_SCREEN) {
        let cursor_line = grid.cursor.point.line.0;
        let mut last = cursor_line;
        'find: for line in (cursor_line + 1..rows as i32).rev() {
            for col in 0..grid.columns() {
                if grid[Line(line)][Column(col)].c != ' ' {
                    last = line;
                    break 'find;
                }
            }
        }
        shift = rows as i32 - 1 - last;
    }
    // With a short history, the view starts above the first line. `draw_pane` leaves those rows empty.
    -(grid.display_offset() as i32) - shift
}

/// Direction of the split that has the leaf `id` as a direct child.
fn parent_split(node: &Node, id: usize) -> Option<bool> {
    match node {
        Node::Leaf(_) => None,
        Node::Split { side_by_side, a, b, .. } => {
            if matches!(**a, Node::Leaf(l) if l == id) || matches!(**b, Node::Leaf(l) if l == id) {
                Some(*side_by_side)
            } else {
                parent_split(a, id).or_else(|| parent_split(b, id))
            }
        },
    }
}

/// Local time as "07:51 AM".
fn clock() -> String {
    // SAFETY: SYSTEMTIME is plain data, and GetLocalTime fills all of it.
    let time = unsafe {
        let mut time = std::mem::zeroed();
        windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut time);
        time
    };
    let hour = if time.wHour % 12 == 0 { 12 } else { time.wHour % 12 };
    format!("{hour:02}:{:02} {}", time.wMinute, if time.wHour < 12 { "AM" } else { "PM" })
}

/// Fills the window with the background color through GDI. bebo uses this before the GPU is ready.
fn paint_background(window: &Window, color: Rgb) {
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
        let brush = CreateSolidBrush(color[0] as u32 | (color[1] as u32) << 8 | (color[2] as u32) << 16);
        FillRect(dc, &rect, brush);
        DeleteObject(brush);
        ReleaseDC(hwnd, dc);
    }
}

/// Start and end of the word at `index`. A space, a quote, or a bracket ends the word.
fn token_range(chars: &[char], index: usize) -> Option<(usize, usize)> {
    let stop = |c: char| c.is_whitespace() || "\"'<>()[]{}|".contains(c);
    if chars.get(index).is_none_or(|c| stop(*c)) {
        return None;
    }
    let start = chars[..index].iter().rposition(|c| stop(*c)).map_or(0, |i| i + 1);
    let mut end = chars[index..].iter().position(|c| stop(*c)).map_or(chars.len(), |i| index + i);
    // Punctuation at the end of a sentence is not a part of the word.
    while end > start && ".,;:!?".contains(chars[end - 1]) {
        end -= 1;
    }
    (end > start).then_some((start, end))
}

#[cfg(test)]
fn token_at(chars: &[char], index: usize) -> String {
    token_range(chars, index).map_or(String::new(), |(start, end)| chars[start..end].iter().collect())
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

fn resolve(color: Color, colors: &Colors, theme: &Theme) -> Rgb {
    let index = match color {
        Color::Spec(rgb) => return [rgb.r, rgb.g, rgb.b],
        Color::Named(name) => name as usize,
        Color::Indexed(i) => i as usize,
    };
    colors[index].map_or(theme.palette(index), |c| [c.r, c.g, c.b])
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
    render::prof::init();
    let event_loop = EventLoop::<UserEvent>::with_user_event().build().expect("event loop");
    event_loop.set_control_flow(ControlFlow::Wait);
    // Start the GPU on a second thread now. It is the slowest step of the start.
    let (gpu_tx, gpu_rx) = std::sync::mpsc::channel();
    let gpu_proxy = event_loop.create_proxy();
    std::thread::spawn(move || {
        let _ = gpu_tx.send(render::init_device(start));
        let _ = gpu_proxy.send_event(UserEvent::GpuReady);
    });
    let state = config_dir().and_then(|d| std::fs::read_to_string(d.join("settings.txt")).ok()).unwrap_or_default();
    let (settings, bottom) = Settings::parse(&state);
    let profiles = Profile::available();
    // `--profile` and `--cwd` come from "Open as administrator".
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1));
    let first_profile = arg("--profile")
        .and_then(|value| Profile::ALL.into_iter().find(|p| format!("{p:?}") == *value))
        .or(settings.profile)
        .filter(|p| profiles.contains(p));
    if let Some(folder) = arg("--cwd") {
        // The first shell starts in the folder of the process.
        let _ = std::env::set_current_dir(PathBuf::from(folder).components().collect::<PathBuf>());
    }
    let mut app = App {
        proxy: event_loop.create_proxy(),
        start,
        window: None,
        gpu: None,
        gpu_rx,
        panes: HashMap::new(),
        tabs: Vec::new(),
        active: 0,
        next_id: 0,
        pane_rects: Vec::new(),
        dividers: Vec::new(),
        mods: ModifiersState::empty(),
        mouse: (-1.0, -1.0),
        press: None,
        drag_divider: None,
        drag_minimap: None,
        last_click: (start, Point::new(Line(0), Column(0)), 0),
        last_title_click: start,
        last_tab_click: (start, usize::MAX),
        clipboard: None,
        profiles,
        theme: settings.theme(),
        bottom,
        scale: 1.0,
        palette: None,
        colon: None,
        find: None,
        menu: false,
        confirm: None,
        hits: Vec::new(),
        hover: None,
        quit: false,
        settings,
        settings_page: 0,
        font_menu: None,
        anim: Anim::default(),
        last_frame: start,
        dirty: false,
        anim_due: false,
        last_profile: start,
        key_at: None,
        echoed: false,
        last_input: start,
        hover_cell: None,
        hover_link: None,
        // The program file has two icons: 1 for the dark mode of Windows and 2 for the light mode. See build.rs.
        icon: Icon::from_resource(if system_dark() { 1 } else { 2 }, None).ok(),
        // SAFETY: the function has no arguments and only reads the token of this process.
        admin: unsafe { windows_sys::Win32::UI::Shell::IsUserAnAdmin() != 0 },
    };
    render::tick(start, "event loop");
    // Start the shell before the window and the GPU. The shell starts while the GPU starts.
    app.new_tab(first_profile);
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

#[cfg(test)]
mod ui_tests {
    use super::*;

    #[test]
    fn colon_popup_lists_commands_and_restart_arguments() {
        let names = |text: &str| colon_items(text).into_iter().map(|(n, ..)| n).collect::<Vec<_>>();
        assert_eq!(names("t"), [":tab", ":theme"]);
        assert_eq!(names("restart"), ["restore", "force", "clean"]);
        assert_eq!(names("restart f"), ["force"]);
        assert_eq!(names("rename my tab"), [":rename"]);
        assert!(names("zzz").is_empty());
    }

    #[test]
    fn fuzzy_finds_characters_in_order() {
        assert_eq!(fuzzy("Restart shell", "rsc"), None);
        assert_eq!(fuzzy("Restart shell \u{b7} clean", "rsc"), Some(vec![0, 2, 17]));
        assert_eq!(fuzzy("New tab", ""), Some(vec![]));
    }

    #[test]
    fn settings_text_reads_back_the_same() {
        let (mut settings, bottom) = Settings::parse("theme=light\nposition=bottom\ncursor=block\nfont_size=99\nbad line\n");
        assert!(bottom && settings.theme == 1 && settings.cursor == 1 && settings.font_px == FONT_MAX && settings.minimap);
        settings.minimap = false;
        settings.profile = Some(Profile::Cmd);
        let (again, bottom) = Settings::parse(&settings.to_text(false));
        assert!(!bottom && again == settings);
    }

    #[test]
    fn animation_value_moves_to_the_target() {
        let mut anim = Anim { on: true, dt: 0.016, ..Default::default() };
        assert_eq!(anim.to(1, 5.0, 0.1), 5.0);
        anim.set(1, 0.0);
        let first = anim.to(1, 10.0, 0.1);
        assert!(first > 0.0 && first < 10.0 && anim.active);
        for _ in 0..100 {
            anim.to(1, 10.0, 0.1);
        }
        assert_eq!(anim.to(1, 10.0, 0.1), 10.0);
        anim.on = false;
        assert_eq!(anim.to(1, 3.0, 0.1), 3.0);
    }

    #[test]
    fn resize_keeps_the_cursor_in_the_screen() {
        use alacritty_terminal::event::VoidListener;
        // Long lines and a short screen: a narrow width moves many wrapped lines into the scrollback.
        for (cols, rows) in [(20, 30), (10, 40), (80, 12), (5, 60), (40, 3), (120, 50), (60, 30)] {
            let mut term = Term::new(Default::default(), &TermSize { cols: 60, rows: 5 }, VoidListener);
            let mut parser: Processor<StdSyncHandler> = Processor::new();
            for i in 0..40 {
                parser.advance(&mut term, format!("line {i} {}
", "x".repeat(50)).as_bytes());
            }
            parser.advance(&mut term, b"PS> ");
            resize_term(&mut term, cols, rows);
            let cursor = term.grid().cursor.point.line.0;
            assert!((0..rows as i32).contains(&cursor), "cursor {cursor} for {cols}x{rows}");
            // More output after the resize must not panic.
            parser.advance(&mut term, b"echo
ok
8[5A[2K");
        }
    }

    #[test]
    fn find_pattern_follows_the_toggles() {
        let mut find = Find { query: "a.b".into(), ..Default::default() };
        assert_eq!(find.pattern(), r"(?i:a\.b)");
        find.regex = true;
        find.case = true;
        find.word = true;
        assert_eq!(find.pattern(), r"(?-i:(?-u:\b)(?:a.b)(?-u:\b))");
    }
}
