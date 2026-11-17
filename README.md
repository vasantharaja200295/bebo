# bebo

bebo is a GPU terminal emulator for Windows, written in Rust. It runs PowerShell 7, Windows PowerShell 5.1, Command Prompt, and WSL in tabs and split panes.

## Features

- **Fast:** a key shows on the screen in about 4 ms. The idle CPU use is zero. The window shows in about 30 ms.
- **Tabs and panes:** split a tab left and right, or top and bottom. Move between panes with the keyboard. Rename a tab with a double-click.
- **Command blocks:** each command shows its duration and its exit code. The minimap shows failed commands and find matches.
- **Links:** a click opens a URL or a file path, also when the link wraps to the next row. URLs have color. A drag selects the text.
- **Find:** search the output as plain text or as a regular expression.
- **Command palette and colon commands:** all actions are in the palette (Ctrl+Shift+P). At the prompt, type `:` for a short list.
- **Settings:** theme (One Dark, One Light, or the Windows theme), terminal font, size, and weight, interface font, cursor shape, tab bar, animations, and the default shell.
- **Readable colors:** each text color has a contrast of at least 4.5:1 (WCAG AA). A unit test checks it.
- **Administrator tabs:** open a shell as administrator from the profile menu.

## Install

1. Get `bebo-<version>-setup.exe` (see [Build the installer](#build-the-installer)).
2. Run it. The wizard installs bebo for the current user. Administrator rights are not necessary.
3. Open bebo from the Start menu.

To remove bebo, use Settings > Apps > Installed apps. The settings stay in `%APPDATA%\bebo`.

The setup file is not signed. If you download it from the internet, Windows SmartScreen shows a warning.

## Keyboard shortcuts

| Action | Keys |
|---|---|
| New tab | Ctrl+Shift+T |
| New tab with shell 1 to 4 | Ctrl+Shift+1 to Ctrl+Shift+4 |
| Next tab | Ctrl+Tab |
| Split left and right | Alt+Shift++ |
| Split top and bottom | Alt+Shift+- |
| Move to the pane on a side | Alt+Left, Alt+Right, Alt+Up, Alt+Down |
| Close pane | Ctrl+Shift+W |
| Find in output | Ctrl+Shift+F |
| Command palette | Ctrl+Shift+P |
| Copy | Ctrl+Shift+C, or Ctrl+C with a selection |
| Paste | Ctrl+Shift+V or Ctrl+V |
| Clear output | Ctrl+K |
| Restart the shell | Ctrl+Shift+R |
| Font size | Ctrl+= and Ctrl+-; Ctrl+0 resets it |
| Settings | Ctrl+, |

Colon commands at the prompt: `:tab`, `:hsplit`, `:vsplit`, `:find`, `:restart`, `:clear`, `:layout`, `:profile`, `:theme`, `:rename <name>`, `:palette`, `:settings`.

## Build from source

You need Rust (edition 2024) and Windows 10 or 11.

```bash
cargo build --release
```

The program is `target\release\bebo.exe`. It needs no other files: the fonts, the icons, and the shell script are inside it.

Run the tests:

```bash
cargo test --release
```

### Build the installer

The installer is an Inno Setup wizard. Inno Setup is necessary only on the build PC.

```bash
winget install JRSoftware.InnoSetup
```

```bash
powershell -ExecutionPolicy Bypass -File installer\build.ps1
```

The result is `dist\bebo-<version>-setup.exe`.

## Environment variables

| Variable | Effect |
|---|---|
| `BEBO_BACKEND=dx12` | Use DirectX 12, not OpenGL. It uses more memory and starts slower. |
| `BEBO_GPU=low` | Use the integrated GPU. |
| `BEBO_VSYNC=1` | Wait for vsync before each frame. |
| `BEBO_CONFIG=<folder>` | Read and write the settings in another folder. |
| `BEBO_PROFILE=<file>` | Write frame times, latency, and heap size to a file each second. |
| `BEBO_TIMING=1` | Write the start times to `%TEMP%\bebo-timing.txt`. |

## How it works

- `alacritty_terminal` parses the shell output and keeps the grid. Windows ConPTY connects the shell.
- `wgpu` draws the frames (OpenGL by default). `glyphon` and `cosmic-text` shape and draw the text.
- `winit` makes the window. bebo draws its own title bar, tabs, popups, and settings page.

## Documents

- [docs/PLAN.md](docs/PLAN.md): the design, each decision, and each measurement.
- [docs/LEARNING-GUIDE.md](docs/LEARNING-GUIDE.md): build a terminal emulator yourself, stage by stage.

## License

bebo is under the MIT License; see [LICENSE](LICENSE).

The interface font Geist is inside bebo under the SIL Open Font License; see [assets/fonts/OFL.txt](assets/fonts/OFL.txt). The icons come from Lucide.
