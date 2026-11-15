# bebo: build plan (primary version)

bebo is a GPU terminal emulator for Windows 11, written in Rust.
This doc is the plan for the primary version. The learning version has its own doc: [LEARNING-GUIDE.md](LEARNING-GUIDE.md).

## 1. Decisions

| Topic | Decision |
|---|---|
| Scope | Terminal emulator only. Zed is the editor. No built-in file editor, LSP, or Tree-sitter. |
| Platform | Windows 11 first. All crates are cross-platform, so Linux and macOS stay possible. |
| Line editing | Shell integration. The shell owns the command line. bebo changes mouse actions into keys. |
| Formatted output | Command blocks, clickable links, colored error lines. JSON and table rendering come later. |
| GPU | wgpu. The default backend is OpenGL on the dedicated GPU, because it starts faster (section 8). `BEBO_BACKEND=dx12` selects DirectX 12. `BEBO_GPU=low` selects the integrated GPU with DirectX 12. |
| Binary | One self-contained `bebo.exe`. It uses the Windows fonts Cascadia Mono or Consolas. |

## 2. Speed budgets

These numbers are the pass/fail limits for each release.

| Measure | Limit |
|---|---|
| Time from process start to first frame | 100 ms |
| Time from process start to shell spawn | 20 ms (the shell starts before the GPU is ready) |
| Key press to pixel change | 1 frame |
| CPU when idle | 0 % (bebo draws only when something changes) |
| RAM with one tab and 10 000 scrollback lines | 80 MB |
| `cat` of a 100 MB log file | No freeze. Input stays responsive. |

Rules that keep bebo fast:

- Start the shell before the window. Start the GPU on a second thread at process start.
- Show the window immediately with the background color. Draw the first GPU frame when the device is ready.
- Read PTY bytes on a reader thread. Do not block the UI thread.
- Draw only on an event (PTY output, input, resize). Do not run a continuous render loop.
- If PTY output is faster than the display, parse all bytes but draw only the latest state.
- Keep a shaped-text cache for each row. Shape a row again only if it changed.

## 3. Architecture

```
            +-------------------- UI thread --------------------+
 keyboard   |  winit event loop                                 |
 mouse ---> |    input.rs   -> bytes to PTY                     |
            |    app.rs     -> tabs, panes, layout              |
            |    render.rs  -> wgpu + glyphon -> window         |
            +---------^------------------------+----------------+
                      | wake-up event          | write
            +---------+----------+     +-------v--------+
            | reader thread      | <-- | ConPTY         | <--> shell (pwsh, cmd, wsl)
            |  scan OSC 133      |     +----------------+
            |  vte parser -> Term (grid + scrollback, behind a lock)
            +--------------------+
```

Crates:

| Job | Crate | Reason |
|---|---|---|
| Window and input | `winit` | Standard. Event-driven. |
| GPU | `wgpu` | OpenGL and DirectX 12 on Windows. Vulkan and Metal on other systems. |
| Text on GPU | `glyphon` | Glyph atlas and text shaping on wgpu. |
| Grid, VT parser, scrollback, selection, regex search, ConPTY | `alacritty_terminal` | One tested crate does all of these jobs. |
| Clipboard | `arboard` | System clipboard. |

Source files:

| File | Job |
|---|---|
| `main.rs` | Start-up order, event loop, input, keybindings, tabs, split panes, click-to-edit (`Region`). |
| `pty.rs` | Spawn the shell, reader thread, OSC 133 scan, restart. |
| `render.rs` | wgpu surface, text rows, rectangles (cursor, selection, tab bar). |
| `shell/bebo.ps1` | Shell integration script for PowerShell. |

## 4. Feature design

### 4.1 Click, select, and replace on the command line

Problem: in a usual terminal, you press arrow keys to go to a word in a long command.

Design:

1. The shell integration script prints OSC 133 marks. Mark `B` shows where the command text starts.
2. bebo knows the cell of mark `B` and the cell of the cursor. The cells between are the command line.
3. **Click** in the command line: bebo counts the cells from the cursor to the click. bebo sends that number of Left or Right keys.
4. **Drag** in the command line: bebo shows a selection.
5. **Type** with a selection: bebo moves the cursor to the selection end, sends Backspace for each selected cell, then sends the typed text.
6. **Delete or Backspace** with a selection: same as step 5 with no text.
7. **Ctrl+Z / Ctrl+Y** on the command line: the shell line editor does undo and redo (PSReadLine, readline).

A click outside the command line does the usual terminal selection.
If the shell has no integration, bebo works as a usual terminal.

Limit: bebo counts cells, not characters. A double-width character or a multi-line command can put the cursor one cell off. Fix this later with a per-cell width count.

### 4.2 Restart command

Use: after an install from the command line, the new `PATH` must be visible.

- Keybinding: `Ctrl+Shift+R`.
- Typed command: `restart`. The integration script defines it. It prints a private escape sequence (`OSC 7717 ; restart`). bebo sees it and restarts the session.

Restart does these steps:

1. Stop the shell of the focused pane.
2. Read a new environment block from the registry (system and user variables). Do not copy the old environment of bebo.
3. Start the same shell in the same directory. ConPTY clears the visible screen when the new shell starts.

Step 2 is the important one. Without it, the new shell gets the old `PATH`.

### 4.3 On-demand tabs

- One tab: no tab bar. The terminal uses the full window.
- `Ctrl+Shift+T` makes tab two. The tab bar shows.
- When one tab stays, the tab bar hides again.

### 4.4 Split panes

- Panes are a binary tree. Each node is a pane or a split (direction and ratio).
- `Ctrl+Shift+D` splits right. `Ctrl+Shift+E` splits down. `Ctrl+Shift+W` closes the pane.
- `Alt+Arrow` moves focus. Drag a divider with the mouse to change the ratio.

### 4.5 Search

- `Ctrl+Shift+F` opens a search bar. The pattern is a regex.
- Search is incremental. Each typed character updates the matches.
- `Enter` goes to the next match. `Shift+Enter` goes to the previous match.
- Scrollback output is read-only, so "replace" applies only to the command line (4.1).

### 4.6 Formatted output

- **Command blocks**: OSC 133 marks give the start and end of each command. bebo draws a thin separator above each prompt.
- A command that takes 1 second or more shows its duration at the right side of its prompt line.
- `Ctrl+Shift+Up/Down` jumps between prompts.
- **Links**: `Ctrl+Click` opens an http(s) URL or a file path that exists. `file.rs:12:5` opens `file.rs`. OSC 8 hyperlinks also work.
- A click on a program or a script (`.exe`, `.bat`, `.ps1`, and others) does not start it. Explorer shows the file.
- **Error marks**: a block with a non-zero exit code gets a red bar in the left margin and shows `exit N`.
- Later: collapsible JSON and aligned tables for a block.

### 4.7 Keybindings

| Key | Action |
|---|---|
| `Ctrl+Shift+C` / `Ctrl+Shift+V` | Copy / paste |
| `Ctrl+C` | Copy if there is a selection. If not, send interrupt to the shell. |
| `Ctrl+V` | Paste |
| `Shift+PageUp/PageDown` | Scroll |
| `Ctrl+Shift+T` / `Ctrl+Tab` | New tab / next tab |
| `Alt+Arrow` | Focus the next or previous pane |
| `Ctrl+Shift+D` / `E` / `W` | Split right / split down / close pane |
| `Ctrl+Shift+F` | Search |
| `Ctrl+Shift+Up/Down` | Go to the previous or next prompt |
| `Ctrl+Click` | Open the link |
| `Ctrl+Shift+R` | Restart session |
| `Ctrl+ +/-/0` | Font size |

## 5. Checklist mapping

The checklist describes an editor. This table shows what each item means for bebo.

| Checklist item | In bebo | Phase |
|---|---|---|
| Instant startup | Speed budgets, shell starts first | 1 |
| Low resource footprint | Event-driven draw, fixed scrollback limit | 1, 6 |
| Large file stability | Large output does not freeze. Fast scrollback search. | 2, 5 |
| Keyboard-first | All actions have a key | all |
| Predictable keybindings | Desktop shortcuts (4.7) | 2 |
| System clipboard | `arboard`, bracketed paste | 2 |
| Mouse support | Click-to-edit, scroll, selection, divider drag, mouse reporting to apps | 2, 3, 4 |
| Syntax highlighting | Shell does it (PSReadLine colors). bebo colors error blocks. | 5 |
| Regex search and replace | Regex search in scrollback. Replace on command line by selection. | 5 |
| Split panes and tabs | 4.3, 4.4 | 4 |
| Undo/redo | Shell line editor on the command line | 3 |
| Easy installation | One `bebo.exe` | 6 |
| LSP, Tree-sitter, plugins | Out of scope. Zed does this. | none |

## 6. Phases

Each phase ends with a binary that works.

| Phase | Content | Done when |
|---|---|---|
| 1 | Window, wgpu, text grid, ConPTY, keyboard, resize, colors, cursor | `pwsh` runs. `vim` and `htop` in WSL show correctly. |
| 2 | Scrollback, selection, clipboard, mouse reporting, font size | Copy and paste work. A 100 MB `cat` does not freeze. |
| 3 | Shell integration (PowerShell), click-to-edit, restart | Click moves the cursor in a long command. `restart` shows a new `PATH`. |
| 4 | On-demand tabs, split panes | Tab bar shows only with two or more tabs. Divider drag works. |
| 5 | Regex search, command blocks, links, prompt jump | Search finds matches in 100 000 lines with no visible delay. |
| 6 | Config file (TOML), bash and zsh integration, release build, speed check against section 2 | All speed budgets pass. |

## 7. Risks

| Risk | Action |
|---|---|
| `glyphon` is too slow for a full-screen redraw | Per-row cache first. If a frame takes more than 4 ms, write a cell-grid glyph atlas renderer. |
| The dedicated GPU is slow to wake up on a laptop | Measure first-frame time. Add the low-power setting. |
| ConPTY changes or delays some escape sequences | Test OSC 133 through ConPTY in phase 3 before other work. |
| A slow PowerShell profile makes the prompt late | bebo cannot fix this. bebo shows the window immediately so the delay is visible as shell time. |

## 8. Status (2026-11-15)

Build: `cargo build --release`. The binary is `target/release/bebo.exe` (7.8 MB).

### Done and tested on this PC

| Item | Result |
|---|---|
| Window, GPU text, colors, cursor, resize | Works |
| ConPTY shell (`pwsh` if installed, if not `powershell`) | Works |
| Shell integration marks through ConPTY (OSC 133) | Works |
| Click moves the cursor in the command line | Works |
| Drag selection in the command line, typed text replaces it | Works |
| `restart` command gives a new environment | Works. A new user variable was visible after `restart`. |
| On-demand tab bar, split panes, focus outline | Works (drawn with the `BEBO_TEST` aid) |
| 60 000 lines of output | No freeze. Input stays responsive. |
| Command blocks: separators, red bar and `exit N` for a failed command, duration | Works |
| Unit tests (`cargo test`) | 5 pass |

### Done but not tested by a real key press or mouse

The test tool cannot press `Ctrl+Shift` keys. Test these by hand:

- Keybindings with `Ctrl` and `Shift` (tabs, splits, close, restart, copy, paste, font size).
- Divider drag, tab click, mouse wheel, double-click and triple-click selection.
- Regex search (`Ctrl+Shift+F`). The search bar draws. The match selection is not tested.
- Mouse reporting to programs such as `vim` and `htop`.
- Prompt jump (`Ctrl+Shift+Up/Down`) and `Ctrl+Click` links. A unit test covers the link text rules.

### Measured speed (release build, GTX 1650 + integrated AMD GPU, Windows PowerShell 5.1)

| Measure | Limit | OpenGL (default) | DirectX 12 |
|---|---|---|---|
| Shell spawn | 20 ms | 30 ms | 30 ms |
| Window visible | none | 100 to 155 ms | 90 ms |
| First GPU frame | 100 ms | 295 to 375 ms | 580 to 660 ms |
| Shell prompt ready | none | 410 to 515 ms | 430 ms |
| RAM, one tab | 80 MB | 70 to 80 MB | 157 MB |
| RAM, 10 000 scrollback lines | 80 MB | 106 MB | not measured |
| Idle CPU | 0 % | about 1 % of one core | 0 % |

The first frame limit of 100 ms fails. The cause is the GPU device start in `wgpu`.
With OpenGL, the first frame is ready before the shell prompt, so the prompt does not wait for the GPU.
To reach 100 ms, replace `wgpu` with direct Direct3D 11 calls. This is a large change.

### Not done

- Collapsible JSON and aligned tables for a block.
- Underline for a link when the mouse is on it.
- Phase 6: config file, bash and zsh integration, installer.
- `Shift+Arrow` selection in the command line.
- Font inside the binary.

### Known limits

- Click-to-edit counts cells. A multi-line command with a continuation prompt can put the cursor in the wrong column.
- After clear-screen (`cls`, `Ctrl+L`), click-to-edit is off until the next prompt.
- A click after the end of the text sends Right keys. With PSReadLine prediction text, Right accepts the prediction.
- `Alt+Arrow` moves focus in tree order, not by direction.
- A window resize removes the block marks of old commands.
- Block marks are off when the scrollback is full (10 000 lines).
- An empty Enter makes a block with no exit status.

### Test aids

| Variable | Effect |
|---|---|
| `BEBO_TIMING=1` | Writes start-up step times to `%TEMP%\bebo-timing.txt`. |
| `BEBO_TEST=tdef` | Runs commands at start: `t` tab, `d` split right, `e` split down, `f` search. |
| `BEBO_BACKEND=dx12` | Uses DirectX 12. |
| `BEBO_GPU=low` | Uses the integrated GPU (DirectX 12 only). |
