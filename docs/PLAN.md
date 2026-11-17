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

## 9. Design build (2026-11-16)

This section is the status of the build from the reference designs. Where it and an earlier section are different, this section is correct.

### Source files

| File | Job |
|---|---|
| `main.rs` | App state, commands, input, tabs, panes, restart, find, colon popup logic. |
| `ui.rs` | All drawing: title bar, panes, minimap, palette, popups, badges. |
| `render.rs` | GPU renderer: rounded rectangles, text in three fonts, two text layers. |
| `theme.rs` | One Dark and One Light colors. |
| `pty.rs` | Shell profiles, ConPTY, shell marks, new environment. |
| `shell/bebo.ps1` | PowerShell integration: marks, prompt colors, command colors, restart banner. |

### Done and tested in the window

| Design item | Result |
|---|---|
| Custom title bar: logo, `bebo / shell title`, window buttons | Works |
| Tabs in the title bar with two or more tabs, shell badge, close button, `+`, profile menu | Works |
| One Dark and One Light themes | Works |
| Colon popup at an empty prompt (`:t` lists `:tab` and `:theme`) | Works |
| `:restart` arguments `restore`, `force`, `clean` | Works |
| Restart divider line and badge (`content restored`, `content lost · opened in home`) | Works |
| Command palette with fuzzy filter and colored matches | Works |
| Find bar: count, all matches colored, current match brighter, minimap marks | Works |
| Prompt position top and bottom (`:layout`) | Works |
| Split panes: accent line on the focused pane, other panes dimmer | Works |
| Minimap lane: view position, failed commands (red), find matches (yellow) | Works |
| Hint strip at an empty prompt, accent bar at the current prompt | Works |
| Theme and prompt position stay after a restart of bebo (`%APPDATA%\bebo\state.txt`) | Works |
| Unit tests (`cargo test`) | 8 pass |

### Done but not tested by a real key press or mouse

- All key chords: `Ctrl+Shift+P` palette, `Ctrl+Shift+T`, `Alt+Shift+-`, `Alt+Shift+=`, `Ctrl+Shift+1` to `4`, `Ctrl+Shift+R`.
- Window drag by the title bar, double-click to maximize, resize by the window edges, the three window buttons.
- The restart confirm box when a command runs.
- Find toggles (`Aa`, `ab`, `.*`), Enter and Shift+Enter in find, a click in the minimap.
- Arrow keys and Tab in the colon popup and the palette. Rename tab.
- Command Prompt and WSL profiles.

### Not built from the designs

| Design item | Note |
|---|---|
| Settings page | Built. See section 10. |
| Color output (paths, tables, JSON in command output) | Not built. The script sets the prompt colors and the PSReadLine command colors only. |
| Warning marks in the minimap | Not built. The minimap has no data for warnings. Red marks come from the exit code. |
| Minimap wide mode on hover, with the line label | Not built. |
| Restart sweep line and dim of old text | Not built. A restart takes less than 150 ms. |
| Blue dot on a tab with new output | Not built. |
| Popup shadows | Not built. Popups have a border. |
| App icon | Skipped, as agreed. |
| Inter font | bebo uses Segoe UI for the interface. The Inter file on this PC is 13 MB, which makes the start slow. |

### Differences from the designs

- With one tab, the title bar has no `+` button. Use `Ctrl+Shift+T`, `:tab`, or the palette.
- `:profile` opens the palette with the shell list. A profile opens in a new tab.
- In the top layout, a restart with restore puts the old content in the scrollback. Scroll up to see it. In the bottom layout, the old content stays visible above the divider.
- The snap layout menu of Windows 11 does not show on the maximize button. `Win+Arrow` keys work.

### Known limits

- A colon typed before the prompt is ready goes to the shell.
- The exit code of a failed PowerShell command can be the code of an earlier program.
- The minimap marks use the scrollback position. In the bottom layout with a short history, they are not next to their lines.
- Find shows 2000 matches maximum.

### Measured speed (release build)

| Measure | Result |
|---|---|
| Window visible | 110 ms |
| First GPU frame | 260 ms |
| Shell prompt ready | 440 ms |
| RAM, one tab | 71 MB |
| Idle CPU | 0 % |
| Binary size | 7.9 MB |

### Test aids

`BEBO_TEST` runs commands at start: `t` tab, `d` split right, `e` split down, `f` find, `p` palette, `m` profile menu, `l` light theme, `b` bottom layout.

## 10. Settings page (2026-11-16)

Open it with `Ctrl+,`, `:settings`, the palette, or the profile menu. Esc closes it.
bebo writes each change immediately to `%APPDATA%\bebo\settings.txt`. The file has `key=value` lines and you can edit it.
The old file `state.txt` is not read.

| Page | Setting | Values |
|---|---|---|
| Appearance | Theme | One Dark, One Light, System (the app mode of Windows, read at start) |
| Appearance | Prompt position | Top, Bottom |
| Appearance | Command popup | Auto, Always below, Always above |
| Appearance | Font | A click selects the next installed font: JetBrains Mono NL, Cascadia Mono, Consolas |
| Appearance | Font size | 10 to 24 px |
| Appearance | Cursor | Beam, Block, Underline |
| Appearance | Tab bar | Auto, Always, Never |
| Appearance | Minimap | On, Off |
| Appearance | Clickable paths | On, Off |
| Appearance | Command strip | On, Off |
| Startup | Default shell | The installed shells. It applies at the next start. |
| Keys | Key list | Read only |

The Appearance page has a preview of the theme, the font, and the cursor.

### Tested in the window

- The page draws in One Dark and One Light.
- Clicks on Theme, Cursor, Font size, and Minimap changed the page, the preview, and the file.
- A new start applied the theme, the cursor, the font size, and the minimap setting.
- Unit tests: 9 pass. One test reads and writes the settings text.

### Not tested

- `Ctrl+,` and Esc.
- Prompt position, Command popup, Font, Tab bar, Clickable paths, Command strip, and Default shell controls.
- The Keys page.

### Differences from the design

- The pages Profiles, Terminal, and Command strip are not built. Command strip is a switch on the Appearance page.
- Font is not a list. A click selects the next font.
- Minimap has On and Off only, because the wide mode is not built.
- Color output and Reduce motion have no switch, because those features are not built.
- The keys cannot be changed.

### Test aids

- `BEBO_CONFIG` selects a different folder for `settings.txt`.
- `BEBO_TEST=s` opens the settings page at start.

## 11. Fixes and features (2026-11-17)

The settings file has a new key `animations`. The key `strip` is not used now.
Section 10 is not fully correct now: the settings are a tab, Font is a list, and Command strip is removed.

### Faults that are fixed

| Fault | Cause | Fix |
|---|---|---|
| The cursor and the colored cells move away from the text after a zoom | bebo rounded the cell width to a full pixel. The text used the exact width. | The cell width is the exact advance of the font. |
| Content is lost after the window becomes small and then large | At a grow, alacritty_terminal moves scrollback lines into the screen. ConPTY then draws its screen on top of them. | bebo moves the lines back to the scrollback. |
| After a resize, the command text shows at a wrong row | PSReadLine calculates a wrong prompt row after a resize. | 150 ms after a resize, bebo sends Ctrl+F12. A key handler in `bebo.ps1` draws the prompt again at the cursor row. |
| bebo stops when a minimized window opens again | A window size of zero went to the layout. | bebo ignores a size of zero. |
| bebo stops if Windows reports a very large window size | wgpu refuses a surface above its size limit. | The size has a limit. |
| The accent bar at the prompt flickers during typing | PSReadLine hides the cursor while it draws the line. The bar was a part of the cursor. | At the prompt, the cursor and the bar show always. |
| The minimap is hard to use | The lane was 9 px wide and the resize edge used 5 px of it. | The lane is 14 px. The thumb is brighter, and brighter again below the mouse. |
| A path has no mark below the mouse | Not built before. | A path or a link below the mouse gets an underline. With Ctrl, the mouse pointer is a hand. |
| The settings page is small in a large window | The row width was fixed. | The rows and the preview use the window width, to a maximum of 1100 px, in the middle. |
| Popup text mixes with the text behind it | All interface text was in one layer. | Popups have their own layer above all other parts. |

### Features

| Feature | How it works |
|---|---|
| Settings tab | `Ctrl+,` opens a tab with the settings. Esc or the close button closes it. |
| Font list | The Font box opens a list of all installed font families. Fixed-width fonts are first. The mouse wheel moves the list. |
| Animations | The caret moves smoothly along its row. Popups fade in and move down 8 px. Settings switches slide. The setting "Animations" stops all of this. |
| Administrator | The shield button in the profile menu, or "Open ... as administrator" in the palette, starts a new bebo window as administrator. Windows asks first. The title bar of that window shows "Administrator". |
| Icons | Lucide icons, from the font in the `lucide-icons` crate. bebo has no SVG renderer: the font has the same shapes. |
| Interface font | Inter, from `Inter.ttc` in the font folder. If it is not installed, Segoe UI. |
| Clear | `Ctrl+K` clears the output. A full-screen program (nano, vim) gets the key as before. |
| New tab and menu buttons | They show also when there is one tab. |
| Hint line | Removed. |

### Large output speed

Measured in bebo with 200,000 JSON lines (35 MB):

| Command | Time |
|---|---|
| `cmd /c type`, before | 34.7 s |
| `cmd /c type`, now | 27.5 s |
| `cmd /c type` in a hidden classic console window, with no terminal program | 34.4 s |
| `Get-Content` | 36.9 s |
| `cat.exe` from Git for Windows | 3.1 s |

- The slow part is the command that prints the file. `type` and `Get-Content` write the text slowly. bebo shows fast output at about 11 MB in a second.
- For a large file, use a fast writer: `& 'C:\Program Files\Git\usr\bin\cat.exe' file`.
- bebo now draws one frame for each 16 ms during output. Before, it drew about 110 frames in a second. This saves CPU.
- Tried and removed: the newer ConPTY of Microsoft (`conpty.dll` and `OpenConsole.exe` 1.25, inside `bebo.exe`). The gain was 5 % to 13 % for the slow commands, and the `cat.exe` output stopped after about 470 lines.

### Tested in the window

- Text and find marks align at 13 px and 15 px, with JetBrains Mono NL and Maple Mono.
- Shrink and grow: lines 40 to 60 stay in order, and the next commands show at the correct row.
- Minimize and restore: bebo continues.
- Minimap drag, link underline, settings tab at 1000 px and 1500 px, font list and font selection, palette above the settings tab, profile menu, `:clear`.
- The shield button started an administrator window.
- Unit tests: 10 pass.

### Tested later with test aids (2026-11-17)

- `Ctrl+K` clears the output. `Ctrl+,` opens the settings tab. `Ctrl+Shift+P` opens the palette.
- Esc closes the font list, and a second Esc closes the settings tab.
- The mouse wheel moves the font list.
- 24 frames during typing at 180 ms and at 40 ms for each character: the accent bar is in each frame, and the caret is at positions between the cells.
- A frame 25 ms after the palette opens shows the palette during its fade.


### Not tested

- The "Administrator" label in the administrator window. A test must not open the Windows consent prompt.
- The prompt redraw after a resize with PowerShell 7, and with a prompt of two or more lines.

### Test aids

- With `BEBO_TEST` set, F9 changes the Ctrl state and F10 changes the Shift state. The window opens without keyboard focus.

### Known limits

- A font that is not fixed-width does not stay on the cell grid.
- The prompt redraw after a resize is not done for a command of two or more lines.
- While a full-screen program runs, a grow of the pane can remove lines of the main screen.
- The font list has no search. Each row uses the interface font, not its own font.

## 12. Small changes (2026-11-17)

| Change | How it works |
|---|---|
| Caret at the start | The caret moves smoothly only in the 400 ms after a key press. At the start and for shell output it goes to its position immediately. |
| New tab folder | A new tab starts in the home folder. The first tab starts in the folder of the bebo process. A split keeps the folder of its pane. |
| Tab name | The name is the profile: PowerShell, Windows PowerShell, Command Prompt, WSL. The program path and the folder name are not used. A program that sets a different title (for example vim) changes the name. |
| Find text | A text longer than the box moves to the left. The end of the text and the caret stay in view. |

Tested in the window: 45 frames at the start show the caret at column 0 and then at the prompt end, with no position between them. The tab name, the home folder of a new tab, and a long find text are correct.

## 13. Tab bar and icons (2026-11-17)

| Change | How it works |
|---|---|
| Crowded tab bar | The tabs share the width. A tab narrower than 80 px shows only its icon, in the middle. |
| Close button in a narrow tab | On the active tab, the close button replaces the icon below the mouse. A tab that is not active keeps its icon, so that a click selects it. |
| Name popover | A tab that has no room for its full name shows the name below the tab, below the mouse. |
| Rename | A double click on a tab opens the rename box with the current name. `:rename name` gives the name directly. `:rename` alone opens the box. |
| Shell icons | Tabs and the profile menu show the icons from the icon pack: PowerShell, Command Prompt, WSL. The clear version for a dark or a light surface follows the theme. |
| bebo icon | The title bar shows the bebo icon. The program file has the dark and the light `.ico`. The window and the taskbar use the one for the app mode of Windows, read at the start. |

- The images are PNG files in `assets/`, in the sizes 16, 20, 24, and 32 px. The build puts them into `bebo.exe`. bebo does not scale images: it draws the size nearest to 16 px times the display scale.
- The renderer draws an image as a custom glyph of glyphon. The `png` crate decodes the files. `build.rs` adds the `.ico` files with the `winresource` crate.
- The interface font is Geist (Regular and SemiBold, version 1.9.2, Open Font License). The two files are in `assets/fonts` and the build puts them into `bebo.exe`. Inter is not used now.
- Limit: with more than about 30 tabs in a 1000 px window, the tabs go behind the window buttons.

Tested in the window: 3 tabs and 25 tabs, the profile menu, the close button and the popover on a narrow tab, a double click rename, `:rename my server`, and the icon of the program file. Unit tests: 10 pass.

## 14. Faster animations (2026-11-17)

- All animations use one fixed time: the value goes 63 % of the distance in 18 ms and stops after about 70 ms. Before, popups used 45 ms and the caret followed the typing speed (8 ms to 60 ms). The typing speed has no effect now.
- The settings page "Keys" has the name "Shortcuts" now.

## 15. Resize faults (2026-11-17)

| Fault | Cause | Fix |
|---|---|---|
| The PTY reader thread stopped with "index out of bounds", and then the pane showed no prompt and no cursor | After a resize that changed the width and the height, bebo moved scrollback lines with the old scrollback size. The cursor row became negative. | `resize_term` moves lines back to the scrollback only when the width does not change, and never more than the cursor row. Test: `resize_keeps_the_cursor_in_the_screen`. |
| The prompt showed two times after a resize of a narrow window | The prompt redraw used the cursor row, but a long prompt and command use two or more rows. With a width change, ConPTY also draws the prompt itself. | bebo sends the redraw key only after a resize that kept the width. The script redraws only a prompt and a command that use one row. |

- In a narrow pane, Windows PowerShell 5.1 breaks long names inside the Name column of `ls`. bebo gives PowerShell the correct width (tested: 60 columns). PowerShell 7 shortens the names with "…" instead.
- Limit: after a resize that kept the width, a prompt and a command of two or more rows can show at a wrong row until the next prompt.

## 16. Small display faults (2026-11-17)

| Fault | Fix |
|---|---|
| The caret showed on the last character of a full row, one cell too far left | After a character in the last column, the cursor waits for the next character before it wraps. bebo now draws the caret after that character. |
| The exit label covered the command text in a narrow pane | The label shows only if its cells on that row are empty. |
| A stop with Ctrl+C showed as a red failure (`exit -1073741510`) | That exit code shows as a dim "stopped" label, with no red mark in the minimap. |
| The prompt bar showed only on the cursor row of a command of two or more rows | The bar goes from the first row of the prompt to the cursor row. |

Checked in the window: dark and light theme, split panes, profile menu, palette, find bar, settings tab, colon popup, restart, a full row of input, and exit labels.

Known: after a restart, the new ConPTY clears the screen. The old content goes to the scrollback, so the screen is empty except for the banner and the prompt.

## 17. Final test pass (2026-11-17)

About 35 test runs with edge cases. No run stopped bebo: bad regex in find, find with no matches, palette with no matches, unknown colon command, a very small window, a 3000-character line, a full-screen program with a resize, `exit` in the last tab, `exit` with the settings tab open, split toggles, font size at the limits, paste of two lines, restart with a running command (Enter and Esc), rename, five tabs with a close in the middle, Command Prompt and WSL tabs, selection and copy, Ctrl+C on a running command.

| Fault | Fix |
|---|---|
| Popups showed through each other: the text of the profile menu was on top of the palette | Each popup has its own drawing layer. The renderer draws the rectangles and the text of each layer before the next layer. The palette also closes the profile menu. |
| Chinese, Japanese, and Korean characters showed as boxes | bebo loads Microsoft YaHei, Yu Gothic, Malgun Gothic, and Nirmala UI as fallback fonts. fontdb maps the files, so memory use stays low. |
| Wide characters moved the text after them off the cell grid | Each wide character is a separate text row at its own cell. |
| `[Console]::Write` and native programs gave `???` for characters outside the code page | The shell script sets the console output to UTF-8. |
| `Ctrl+=` and `Ctrl+-` went past the limits of the settings page, and the value was not saved | One range, 8 to 32 px, for the keys and the settings page. The keys save the value. |
| When the active tab closed, the tab on its left became active | The tab on its right becomes active, as in a web browser. |
| A window could become too small for the title bar and the popups | The minimum window size is 420 x 240. |

Not a bebo fault: a narrow pane with `ls` in Windows PowerShell 5.1 (section 15).

## 18. Profile and speed (2026-11-17)

bebo has a built-in profiler. Set `BEBO_PROFILE` to a file path. bebo then writes a table every second: each step, its count, total, mean, and maximum time in microseconds. It also counts the PTY read sizes, the wake-ups, and the time from a key to the screen. With the variable not set, the cost is one atomic read for each step.

Measured on the test PC (GTX 1650, OpenGL), release build:

| Case | Before | After |
|---|---|---|
| Key to screen, mean | 19.8 ms | 4.3 ms |
| Key to screen, maximum | 29.9 ms | 18.0 ms |
| Key to shell output, mean | 10.1 ms | 1.4 ms |
| Frame time with present, mean | 9.6 ms | 2.8 ms |
| CPU at idle, 5 s | 0 ms | 0 ms |
| 200,000 lines with `cat.exe` | 1.9 s CPU, 244 frames | 2.0 s CPU, 231 frames |
| Memory at idle | 83 MB | 74 MB |
| First frame after start | 305 ms | 305 ms (not changed) |

Changes:

- **No vsync wait.** With vsync, each present call waited about 8 ms, and the event loop waited with it. Keys and shell output waited behind that frame. bebo now uses Mailbox or Immediate. Windows composes each window, so a window does not tear. `BEBO_VSYNC=1` selects vsync again.
- **Own frame limits.** Shell output: one frame for each 16 ms. Animations: one frame for each refresh of the display. The echo of a typed key shows immediately.
- **One frame less for each key.** A key that does not change the view (no selection, no scroll) draws no frame. The echo of the shell draws the next frame.
- **Row cache.** Shaped rows stay in the cache until 1500 rows, so a scroll back does not shape the rows again.

Not changed, with the reason:

- The shell prompt shows at about 450 ms. That is the start time of Windows PowerShell 5.1 and the user profile, not bebo.
- DirectX 12 has Mailbox mode, but its start is about 600 ms slower than OpenGL on this PC.
- The large output time is the speed of the program that writes (see section 11).

## 19. Install package and comparison with Windows Terminal (2026-11-17)

### Install package

Run `installer\build.ps1`. It builds bebo and compiles `installer\bebo.iss` with Inno Setup 6 into `dist\bebo-<version>-setup.exe`.
Inno Setup is necessary only on the build PC (`winget install JRSoftware.InnoSetup`). The setup wizard contains everything.

The wizard:

- installs `bebo.exe` to `%LOCALAPPDATA%\Programs\bebo` (no administrator rights). The user can select a different folder, or "all users" with administrator rights,
- adds a Start menu shortcut, and a desktop shortcut if the user selects it,
- adds bebo to "Installed apps" in Windows Settings, with the Inno Setup uninstaller,
- asks the user to close bebo if it is open, and can open bebo on the last page.

The setup .exe is not signed. If you download it from the internet, Windows SmartScreen shows a warning. A code signing certificate removes the warning.
An earlier version used IExpress and PowerShell scripts. They were replaced, because a script installer shows no pages and gives the user no clear result.

bebo needs nothing more on the PC: the fonts, the icons, and the shell script are inside `bebo.exe`.

### Comparison

Windows Terminal 1.24.12741.0 and bebo 0.1.0, both with Windows PowerShell 5.1 and the user profile, on the test PC (GTX 1650). Launch values are the median of 3 starts.

| Item | Windows Terminal | bebo |
|---|---|---|
| Window shows | 548 ms | 28 ms (background color), 291 ms (first full frame) |
| Shell prompt ready | 759 ms | 521 ms |
| Memory at idle (terminal + console host), working set | 111 MB | 91 MB |
| Memory at idle, private | 76 MB | 103 MB |
| CPU at idle, 10 s | 0 ms | 0 ms |
| 200,000 lines (35 MB) with `cat.exe` | 0.6 s | 2.8 s |
| The same file with `cmd /c type` | 20.3 s | 27.5 s |
| Key to screen | not measured | 4.3 ms (built-in profiler) |
| Install size | 17 MB (package) | 9.4 MB (one file) |

- Windows Terminal is faster with large output. It uses its own newer console host (OpenConsole). bebo uses the console host of Windows.
- Section 11 says that the newer console host stopped `cat.exe` output. The first test had faults (see below), so the test was done again with a better method.
- The tests could not measure the key-to-screen time of Windows Terminal.

## 20. Second test of the newer console host (2026-11-17)

The test runs the command through the test aid `BEBO_RUN`: the shell script runs the command before the first prompt, so no typing is necessary. The command writes its time to a file. Same 35 MB file.

| Command | Console host of Windows | Newer console host 1.25 (OpenConsole) |
|---|---|---|
| `cat.exe` | 2.8 s, 2.9 s, 2.8 s | stopped, stopped, stopped |
| `cmd /c type` | 25.6 s | 22.4 s |
| `Get-Content` | 34.8 s | 30.9 s |

- With the newer console host, `cat.exe` output stops after about 115 KB in each run. This is a real fault, not a test fault.
- At start, the newer console host sends `ESC [1t`, `ESC [c`, `ESC [?1004h`, `ESC [?9001h`, and `ESC [6n`. bebo answers the cursor position request. The cause of the stop is not found yet. Windows Terminal uses the same host with no stop, so it answers or does something that bebo does not.
- Decision: bebo keeps the console host of Windows. The gain of 10 to 12 % for slow writers does not justify the risk.

Faults in the earlier tests, for the record:

- The test tool sends "~" as Enter, and the short path of the test folder has a "~". The typed command was cut.
- A test window behind other windows does not show new frames. A screenshot then shows an old frame or an empty window.

Test aid: `BEBO_RUN` holds a PowerShell command. The bebo script runs it before the first prompt.

## 21. Memory (2026-11-17)

The profiler now counts the heap of bebo: a counting allocator adds and removes the size of each allocation. `BEBO_PROFILE` writes the current and the maximum heap. The GPU driver has its own memory, so it is not in this count.

| Case | Heap of bebo | Private memory of the process |
|---|---|---|
| Idle, one tab | 8.1 MB | 87 MB |
| Five tabs | 36.7 MB | 120 MB |
| After 200,000 lines (full scrollback), before | 52.6 MB (maximum 75.6 MB) | 127 MB |
| After 200,000 lines, after the change | 43.2 MB (maximum 44.8 MB) | 125 MB |

- At idle, about 79 MB of the 87 MB is not bebo: the NVIDIA OpenGL driver (`nvoglv64.dll`, `nvgpucomp64.dll`) and Windows libraries.
- Each tab adds about 7 MB. About 4 MB of that is fixed buffers in the libraries: two 1 MB pipe buffers (alacritty_terminal) and a 2 MB buffer for synchronized output (vte). bebo cannot change them without a copy of the libraries.
- A full scrollback of 10,000 lines at 120 columns uses about 29 MB.
- Change: the cache of shaped rows has a limit of 400 rows (about 10 screens), not 1500. That saves about 10 MB after large output. Scrolling back more than 10 screens shapes the rows again, at about 30 us for each row.

## 22. Links, fonts, and pane keys (2026-11-17)

- **Wrapped links.** A link that the terminal wraps to the next row is one link. `logical_line` joins the rows that have the `WRAPLINE` flag (at most 16 rows up and down). Before, a click on a wrapped URL opened only the part on one row.
- **Links open with a click.** A click with no drag on a link opens it. The mouse pointer is a hand on every link. A drag still selects the text. In the command line that you type, a click moves the shell cursor, as before. In a program that gets the mouse (vim, less), only Ctrl+click opens a link.
- **Link color.** Every http and https URL in the view, and every OSC 8 hyperlink, has the accent color. A URL starts at `http`, also after other text, for example `url=https://...`. File paths get color only below the mouse, because each path needs a disk check.
- **Font weight.** Settings > Appearance > Font weight: 100 to 900 in steps of 100 for the terminal font. Bold text is 300 heavier. Only the regular and bold files load at the start, so another weight reads the system font folders first (about 100 ms, one time). A font without that weight uses its nearest weight.
- **Interface font.** Settings > Appearance > Interface font: any installed font for the tabs, the menus, and the settings page. The default is Geist.
- **Pane keys.** Alt+Left, Alt+Right, Alt+Up, and Alt+Down move the focus to the pane on that side. With one pane, the shell gets these keys. The command palette has the same commands ("Focus pane left" and the others).
- **Prompt weight.** The prompt rows (the prompt line of each command and the command line now) are one weight step heavier than the output: 400 gives 500 (Medium). The request was +50, but a font file has only steps of 100, and the font search picks the lower weight for 450, so +50 gave no change in a pixel test. The Medium file of the default font loads at the start.
- **Minimize.** Windows sends a resize to 160x28 when it minimizes the window. bebo gave that size to the shell, which then wrapped the prompt into a few columns; after the restore only the end of the prompt was left. bebo now ignores a resize while the window is minimized.

## 23. Contrast (2026-11-17)

Each text color of both themes now has a contrast of at least 4.5:1 (WCAG AA) on the background and on a selected row. The colors that failed moved toward white (dark theme) or black (light theme) until they had 4.6:1, so their hue stays. The test `text_colors_are_readable` in `src/theme.rs` checks it.

| Theme | Color | Before | After |
|---|---|---|---|
| Dark | Secondary text (`dim`) | 3.5 | 4.6 |
| Dark | Bright black (ANSI 8), grey text of many tools | 3.7 | 4.6 |
| Dark | Red | 4.1 | 4.6 |
| Dark | Text on a find match | 4.1 | 4.7 |
| Light | Accent blue | 3.2 | 4.6 |
| Light | Red, yellow, green, cyan, blue (ANSI) | 2.5 to 4.0 | 4.6 |

Black and white (ANSI 0, 7, 15) do not change: programs also use them as background colors.

## 24. macOS (2026-11-17)

bebo builds for macOS 11 or later, for Apple Silicon and Intel. `src/platform.rs` has the parts that are different on each system.

| Part | Windows | macOS |
|---|---|---|
| GPU | OpenGL (DirectX 12 with `BEBO_BACKEND=dx12`) | Metal |
| PTY | ConPTY | Unix PTY |
| Shells | PowerShell 7, Windows PowerShell, Command Prompt, WSL | zsh, bash, PowerShell 7 |
| Shell marks | `bebo.ps1` | `bebo.zsh` (as `.zshenv` through ZDOTDIR), `bebo.bash` (as `--rcfile`) |
| Window frame | bebo draws the buttons and resizes at the edges | the system draws the buttons and resizes; the title bar is transparent |
| Command key | Ctrl (and Ctrl+Shift) | Command; Control and Option go to the shell |
| Fonts at the start | a short list of files | all system fonts |
| Open a link | `explorer.exe` | `open` (`open -R` shows a program file) |
| Settings | `%APPDATA%\bebo` | `~/Library/Application Support/bebo` |
| Administrator tabs | yes | no |
| Package | Inno Setup wizard | `bebo.app` in a disk image, signed ad hoc |

- Checks on the Windows PC: `cargo check` for `aarch64-apple-darwin` and `x86_64-apple-darwin`, also for the tests, with no errors and no warnings. The Windows build and its tests did not change.
- The GitHub Actions workflow `macOS` runs the tests on a Mac, builds `bebo.app`, and keeps the disk image as an artifact.
- Not tested on a real Mac: the window, the keys, the zsh and bash marks, and the fonts. Test these on a Mac before you give the app to other people.
- The app has no Developer ID signature and is not notarized. macOS stops the first start; the user opens it from System Settings > Privacy & Security. Notarization needs a paid Apple Developer account.

## 25. Move tabs (2026-11-18)

A press on a tab starts a drag. The tab follows the mouse on a layer above the other tabs. When the mouse passes over another place, the tabs change places, and the other tabs slide to their new places (the same animation as the popups). Only the x position counts. After the release, the tab slides into its place. The hit rectangles stay at the places, not at the animated positions, so the order changes only at fixed points. Each tab has a fixed id for its animation, because its index changes. A click with no move still selects the tab, and a double click still renames it.

## 26. Start speed (2026-11-18)

With the settings of the test user (terminal font SF Mono, weight 500, interface font SF Pro Display), the window was empty for about 580 ms. Medians of 5 starts on the test PC:

| | Before | After |
|---|---|---|
| Window with its first frame | ~100 ms (empty) | 31 ms (GDI title bar) |
| GPU frame | 683 ms | 295 ms |
| Prompt | ~460-530 ms | 371 ms |

1. **Font list.** A font that is not in the short start list made bebo read all 589 system font files (about 430 ms). bebo now writes `fonts.txt` (family and file) next to `settings.txt` after a full scan, and loads only the files of the fonts in use. A font that the list does not have (a new font) gives one full scan again.
2. **Font thread.** The fonts load, and their first text layout runs, on a second thread while the GPU starts.
3. **Start order.** The GPU thread loads the OpenGL driver, and Windows loads only one DLL at a time. The administrator check (shell32.dll) then waited about 48 ms, before the shell started. The shell now starts first (at about 34 ms), then the GPU thread, then the icon and the administrator check.
4. **PowerShell script.** The key handler and the colors of PSReadLine cost about 60 ms. They now run at the first line read, after the prompt is on the screen.
5. **GDI first frame.** The window opens first, at about 30 ms, and GDI draws the title bar at once: the icon, the name, the tab name, the new tab and menu buttons, and the window buttons, at the same places as the GPU frame (`platform::paint_first_frame`). Geist goes to GDI with `AddFontMemResourceEx`. The GPU frame replaces it at about 300 ms; only the shell badge is new then.
6. **Shell thread.** The first shell starts on a second thread in `main`, while the window opens. Median of 10 starts: prompt at 371 ms (395 ms before).
7. **Administrator check.** The elevation of the process token, not `IsUserAnAdmin`, which loads shell32.dll.

What remains: the NVIDIA OpenGL driver needs about 185 ms, and Windows PowerShell 5.1 needs about 200 ms to start (65 ms more for PSReadLine).
