//! Shell process, ConPTY, shell integration marks, and a new environment for each shell.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use alacritty_terminal::event::{Event, EventListener, OnResize, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty::{self, ChildEvent, EventedPty, EventedReadWrite, Options, Shell};
use polling::{Event as PollEvent, PollMode, Poller};
use winit::event_loop::EventLoopProxy;

/// A mark that the shell integration script prints.
#[derive(Debug, PartialEq)]
pub enum Mark {
    /// OSC 133;B. The command text starts at the cursor.
    InputStart,
    /// OSC 133;C. The shell runs the command.
    Exec,
    /// OSC 133;D. The command ended with this exit code.
    Done(i32),
    /// OSC 7717;restart. The user typed `restart`.
    Restart,
    /// OSC 7717;cwd. The current directory of the shell.
    Cwd(String),
}

pub enum UserEvent {
    /// (pane id, shell generation, event)
    Term(usize, u32, Event),
    Mark(usize, Mark),
    /// The GPU device is ready on the channel.
    GpuReady,
}

#[derive(Clone)]
pub struct Proxy {
    pub id: usize,
    pub generation: u32,
    pub proxy: EventLoopProxy<UserEvent>,
}

impl EventListener for Proxy {
    fn send_event(&self, event: Event) {
        let _ = self.proxy.send_event(UserEvent::Term(self.id, self.generation, event));
    }
}

pub struct TermSize {
    pub cols: usize,
    pub rows: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

pub type SharedTerm = Arc<FairMutex<Term<Proxy>>>;

pub fn new_term(proxy: Proxy, cols: usize, rows: usize) -> SharedTerm {
    let config = Config { scrolling_history: 10_000, ..Config::default() };
    Arc::new(FairMutex::new(Term::new(config, &TermSize { cols, rows }, proxy)))
}

/// Finds OSC 133 and OSC 7717 sequences in the PTY byte stream. A sequence can go across two reads.
#[derive(Default)]
pub struct OscScan {
    state: u8, // 0 text, 1 after ESC, 2 in OSC, 3 after ESC in OSC
    buf: Vec<u8>,
}

impl OscScan {
    pub fn feed(&mut self, bytes: &[u8], mut out: impl FnMut(Mark)) {
        for &b in bytes {
            match (self.state, b) {
                (0, 0x1b) => self.state = 1,
                (1, b']') => {
                    self.state = 2;
                    self.buf.clear();
                },
                (1, 0x1b) => {},
                (1, _) => self.state = 0,
                (2, 0x07) => {
                    self.finish(&mut out);
                    self.state = 0;
                },
                (2, 0x1b) => self.state = 3,
                (2, _) if self.buf.len() < 1024 => self.buf.push(b),
                (3, _) => {
                    if b == b'\\' {
                        self.finish(&mut out);
                    }
                    self.state = 0;
                },
                _ => {},
            }
        }
    }

    fn finish(&mut self, out: &mut impl FnMut(Mark)) {
        let text = String::from_utf8_lossy(&self.buf);
        let mut parts = text.splitn(3, ';');
        let mark = match (parts.next(), parts.next(), parts.next()) {
            (Some("133"), Some("B"), _) => Mark::InputStart,
            (Some("133"), Some("C"), _) => Mark::Exec,
            (Some("133"), Some("D"), code) => Mark::Done(code.and_then(|c| c.parse().ok()).unwrap_or(0)),
            (Some("7717"), Some("restart"), _) => Mark::Restart,
            (Some("7717"), Some("cwd"), Some(path)) => Mark::Cwd(path.to_string()),
            _ => return,
        };
        out(mark);
    }
}

/// A PTY that reports shell integration marks while the event loop reads from it.
struct Tap {
    pty: tty::Pty,
    scan: OscScan,
    proxy: Proxy,
}

impl io::Read for Tap {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.pty.reader().read(buf)?;
        crate::render::prof::count("pty: bytes per read", n as u64);
        let Tap { scan, proxy, .. } = self;
        scan.feed(&buf[..n], |mark| {
            let _ = proxy.proxy.send_event(UserEvent::Mark(proxy.id, mark));
        });
        Ok(n)
    }
}

impl EventedReadWrite for Tap {
    type Reader = Self;
    type Writer = <tty::Pty as EventedReadWrite>::Writer;

    unsafe fn register(&mut self, p: &Arc<Poller>, e: PollEvent, m: PollMode) -> io::Result<()> {
        unsafe { self.pty.register(p, e, m) }
    }
    fn reregister(&mut self, p: &Arc<Poller>, e: PollEvent, m: PollMode) -> io::Result<()> {
        self.pty.reregister(p, e, m)
    }
    fn deregister(&mut self, p: &Arc<Poller>) -> io::Result<()> {
        self.pty.deregister(p)
    }
    fn reader(&mut self) -> &mut Self {
        self
    }
    fn writer(&mut self) -> &mut Self::Writer {
        self.pty.writer()
    }
}

impl EventedPty for Tap {
    fn next_child_event(&mut self) -> Option<ChildEvent> {
        self.pty.next_child_event()
    }
}

impl OnResize for Tap {
    fn on_resize(&mut self, size: WindowSize) {
        self.pty.on_resize(size)
    }
}

/// A shell that bebo can start.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Profile {
    Pwsh,
    PowerShell,
    Cmd,
    Wsl,
}

impl Profile {
    pub const ALL: [Profile; 4] = [Profile::Pwsh, Profile::PowerShell, Profile::Cmd, Profile::Wsl];

    pub fn name(self) -> &'static str {
        match self {
            Profile::Pwsh => "PowerShell 7",
            Profile::PowerShell => "Windows PowerShell 5.1",
            Profile::Cmd => "Command Prompt",
            Profile::Wsl => "WSL",
        }
    }

    /// The title of a tab with this shell.
    pub fn label(self) -> &'static str {
        match self {
            Profile::Pwsh => "PowerShell",
            Profile::PowerShell => "Windows PowerShell",
            Profile::Cmd => "Command Prompt",
            Profile::Wsl => "WSL",
        }
    }

    /// Id of the icon of this shell. See `IMAGES` in main.rs.
    pub fn image(self) -> u16 {
        match self {
            Profile::Pwsh | Profile::PowerShell => 1,
            Profile::Cmd => 2,
            Profile::Wsl => 3,
        }
    }

    pub fn program(self) -> &'static str {
        match self {
            Profile::Pwsh => "pwsh.exe",
            Profile::PowerShell => "powershell.exe",
            Profile::Cmd => "cmd.exe",
            Profile::Wsl => "wsl.exe",
        }
    }

    /// The profiles that are installed on this PC.
    pub fn available() -> Vec<Profile> {
        let path = std::env::var("PATH").unwrap_or_default();
        Profile::ALL.into_iter().filter(|p| find_program(&path, p.program()).is_some()).collect()
    }

    /// PowerShell 7 if it is installed. If not, Windows PowerShell.
    pub fn default_profile() -> Profile {
        let path = std::env::var("PATH").unwrap_or_default();
        if find_program(&path, "pwsh.exe").is_some() { Profile::Pwsh } else { Profile::PowerShell }
    }
}

/// Starts a shell on a new ConPTY. The shell writes into `term`.
/// `banner` is "SGR code|text". The PowerShell script prints it above the first prompt.
pub fn spawn_shell(
    term: SharedTerm,
    proxy: Proxy,
    size: WindowSize,
    cwd: Option<PathBuf>,
    profile: Profile,
    banner: Option<String>,
) -> io::Result<EventLoopSender> {
    let mut env = fresh_env();
    env.insert("TERM_PROGRAM".into(), "bebo".into());
    env.insert("COLORTERM".into(), "truecolor".into());
    if let Some(banner) = banner {
        env.insert("BEBO_BANNER".into(), banner);
    }
    // Test aid: the PowerShell script runs the command in BEBO_RUN before the first prompt.
    if let Ok(run) = std::env::var("BEBO_RUN") {
        env.insert("BEBO_RUN".into(), run);
    }

    let path = env.iter().find(|(k, _)| k.eq_ignore_ascii_case("path")).map(|(_, v)| v.clone()).unwrap_or_default();
    let program = find_program(&path, profile.program()).unwrap_or_else(|| profile.program().to_string());
    let args = match profile {
        Profile::Pwsh | Profile::PowerShell => {
            // The script has no double quotes and no comments in the code part, so one line is safe.
            let script: String = include_str!("shell/bebo.ps1")
                .lines()
                .filter(|l| !l.trim_start().starts_with('#'))
                .collect::<Vec<_>>()
                .join(" ");
            ["-NoLogo", "-NoExit", "-Command", &script].map(String::from).to_vec()
        },
        Profile::Cmd | Profile::Wsl => Vec::new(),
    };

    let options = Options {
        shell: Some(Shell::new(program, args)),
        working_directory: cwd.filter(|p| p.is_dir()),
        drain_on_exit: false,
        env,
        escape_args: true,
    };
    let pty = tty::new(&options, size, 0)?;
    let tap = Tap { pty, scan: OscScan::default(), proxy: proxy.clone() };
    let event_loop = EventLoop::new(term, proxy, tap, false, false)?;
    let tx = event_loop.channel();
    event_loop.spawn();
    Ok(tx)
}

fn find_program(path: &str, program: &str) -> Option<String> {
    path.split(';')
        .map(|dir| PathBuf::from(dir.trim()).join(program))
        .find(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
}

/// Reads the environment variables from the registry.
/// A new shell then sees a `PATH` that an installer changed after bebo started.
fn fresh_env() -> HashMap<String, String> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};

    // Use the spelling of the process variable if it exists ("Path" and "PATH" are the same variable).
    let process: HashMap<String, (String, String)> =
        std::env::vars().map(|(k, v)| (k.to_ascii_lowercase(), (k, v))).collect();
    let mut env: HashMap<String, String> = HashMap::new();
    let mut paths: Vec<String> = Vec::new();

    let sources = [
        (HKEY_LOCAL_MACHINE, r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment"),
        (HKEY_CURRENT_USER, "Environment"),
    ];
    for (root, key) in sources {
        let Ok(key) = RegKey::predef(root).open_subkey(key) else { continue };
        for (name, _) in key.enum_values().flatten() {
            let Ok(value) = key.get_value::<String, _>(&name) else { continue };
            let lower = name.to_ascii_lowercase();
            // The machine key has USERNAME=SYSTEM. Keep the value of the process.
            if lower == "username" {
                continue;
            }
            let lookup = |var: &str| {
                let var = var.to_ascii_lowercase();
                env.iter()
                    .find(|(k, _)| k.to_ascii_lowercase() == var)
                    .map(|(_, v)| v.clone())
                    .or_else(|| process.get(&var).map(|(_, v)| v.clone()))
            };
            let value = expand(&value, lookup);
            if lower == "path" {
                paths.push(value);
            } else {
                let name = process.get(&lower).map(|(k, _)| k.clone()).unwrap_or(name);
                env.insert(name, value);
            }
        }
    }
    if !paths.is_empty() {
        let name = process.get("path").map(|(k, _)| k.clone()).unwrap_or("Path".into());
        env.insert(name, paths.join(";"));
    }
    env
}

/// Replaces `%NAME%` with the value from `lookup`. An unknown name stays as it is.
fn expand(value: &str, lookup: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) => match lookup(&after[..end]) {
                Some(v) => {
                    out.push_str(&v);
                    rest = &after[end + 1..];
                },
                None => {
                    out.push('%');
                    rest = after;
                },
            },
            None => {
                out.push('%');
                rest = after;
            },
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_finds_marks_across_reads() {
        let mut scan = OscScan::default();
        let mut marks = Vec::new();
        scan.feed(b"text\x1b]133;D;3\x07\x1b]7717;cwd;C:\\a;b\x07PS> \x1b]13", |m| marks.push(m));
        scan.feed(b"3;B\x1b\\ls\x1b[31m\x1b]0;title\x07\x1b]7717;restart\x07", |m| marks.push(m));
        assert_eq!(marks, vec![
            Mark::Done(3),
            Mark::Cwd("C:\\a;b".into()),
            Mark::InputStart,
            Mark::Restart
        ]);
    }

    #[test]
    fn expand_replaces_known_names_only() {
        let lookup = |n: &str| (n.eq_ignore_ascii_case("root")).then(|| "C:\\W".to_string());
        assert_eq!(expand("%ROOT%\\x;%NOPE%;50%", lookup), "C:\\W\\x;%NOPE%;50%");
    }
}
