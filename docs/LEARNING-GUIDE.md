# Build a terminal emulator by yourself: learning guide

This guide is for your own version. You write it without AI.
It gives you the concepts, the order, and a test for each step. It does not give you the code.

The primary version uses `alacritty_terminal`, which hides the most interesting parts.
In your version, you write those parts. The plan for the primary version is in [PLAN.md](PLAN.md).

## 1. Rules for the learning version

- Do not use `alacritty_terminal`. Write the grid and the escape sequence handler.
- You can use `vte` for the parser state machine in the first pass. Write your own parser in stage 9.
- Start with CPU rendering. Move to the GPU only after the terminal works.
- Make each stage work before you start the next stage.
- Keep a notebook. Write each bug and its cause. This is where most of the learning is.

## 2. What a terminal emulator is

A terminal emulator has three parts:

1. **PTY**: a pipe pair between your program and the shell. The shell thinks it talks to a hardware terminal.
2. **State machine**: the shell sends bytes. Some bytes are text. Some bytes are commands ("move the cursor", "set color red").
3. **Renderer**: draws a grid of cells in a window.

Data flow:

```
key press -> bytes -> PTY -> shell
shell -> bytes -> PTY -> parser -> grid -> renderer -> window
```

The terminal does not know what a "command" or a "prompt" is. It sees only bytes. Remember this for stage 8.

## 3. Stages

### Stage 0: Rust base

You need these parts of Rust:

- Ownership, borrows, lifetimes.
- `enum` and `match` (the parser is a large `match`).
- Traits.
- Threads, `std::sync::mpsc` channels, `Arc<Mutex<T>>`.
- `std::io::Read` and `Write`.

Read: *The Rust Programming Language* (the book), chapters 4, 6, 10, 13, 16.

Test: write a program that starts `cmd.exe` with `std::process::Command`, sends `dir`, and prints the output.
Then find out why `vim` cannot work this way. The answer is the reason a PTY exists.

### Stage 1: PTY

Concepts:

- What a pseudo terminal is. Unix: `openpty`, `fork`. Windows: ConPTY (`CreatePseudoConsole`).
- Why a pipe is not sufficient: programs call `isatty`, ask for the window size, and use raw mode.
- The window size message (`ResizePseudoConsole`).

Work:

1. Use the `portable-pty` crate first. Later, call ConPTY directly with the `windows` crate.
2. Start `pwsh`. Read the output on a thread. Print the raw bytes with `{:?}`.

Test: you see `\x1b[` sequences in the output. Look at them. These are the bytes that you parse next.

Read: Microsoft blog, "Windows Command-Line: Introducing the Windows Pseudo Console (ConPTY)".

### Stage 2: Parser

Concepts:

- C0 control codes: `\r`, `\n`, `\b`, `\t`, `BEL`.
- Escape sequence families: CSI (`ESC [`), OSC (`ESC ]`), DCS, plain ESC.
- The parser is a state machine. One byte goes in at a time. A sequence can be split across two reads.
- UTF-8 decode. One character can be 1 to 4 bytes, also split across reads.

Work:

1. Use the `vte` crate. Implement its `Perform` trait. Print each event (`print`, `execute`, `csi_dispatch`, `osc_dispatch`).

Test: run `ls --color` in WSL. Your program prints "print 'f'", "csi m [1, 34]", and so on.

Read: Paul Williams, "A parser for DEC's ANSI-compatible video terminals" (vt100.net). Read the state diagram until you can draw it.

### Stage 3: Grid

Concepts:

- A cell: character, foreground, background, flags (bold, italic, underline, inverse).
- The cursor: row, column, current attributes.
- Line wrap, and the "pending wrap" state at the last column. This is a classic bug source.
- Scroll region. A new line at the bottom moves the lines up.
- SGR (`CSI m`): 16 colors, 256 colors, true color.
- Erase commands: `CSI J`, `CSI K`. Cursor commands: `CSI H`, `A`, `B`, `C`, `D`.

Work:

1. Make `struct Grid { cells: Vec<Cell>, cols, rows }`. Decide: one flat `Vec` or a `Vec` of rows. Write the reason in your notebook.
2. Connect the parser events to the grid.
3. Write unit tests: feed bytes, then compare the grid to the expected text. Do this before any window exists.

Test: feed `"ab\r\ncd\x1b[1;1HX"`. The grid shows `Xb` on row 1 and `cd` on row 2.

Read: "XTerm Control Sequences" (invisible-island.net). Use it as a reference. Do not read it from start to end.

### Stage 4: Window and CPU rendering

Concepts:

- Event loop (`winit`): events come in, you change state, you ask for a redraw.
- A frame buffer is an array of pixels.
- Font rasterization: glyph, advance, baseline, ascent, descent. A monospace font gives a fixed cell size.

Work:

1. Use `winit` + `softbuffer` (pixel buffer) + `fontdue` (glyph rasterizer).
2. Draw the grid: for each cell, fill the background, then blend the glyph.
3. Connect the keyboard: a key press becomes bytes for the PTY.

Test: you can type `dir` and see the result. This is your first real terminal. Run `vim` in WSL and see what breaks.

### Stage 5: Complete the terminal behavior

Work, in this order:

1. Resize: a new window size gives new `cols` and `rows`. Tell the PTY.
2. Special keys: arrows, Home, End, F-keys, Ctrl and Alt combinations. Find out what "application cursor mode" (`DECCKM`) changes.
3. Alternate screen (`CSI ? 1049 h`). `vim` uses it.
4. Scrollback: lines that leave the top go to a ring buffer.
5. Cursor shapes and blink.

Test: run `vttest` in WSL. Menu items 1 and 2 pass. Run `htop`. It looks correct.

### Stage 6: Selection, clipboard, mouse

Concepts:

- Selection is a pair of grid points. It must stay correct when the grid scrolls.
- Bracketed paste (`CSI ? 2004 h`): why paste needs marks around it.
- Mouse reporting modes (1000, 1002, 1006): the program in the terminal asks for mouse events.

Test: select text with the mouse and paste it in Notepad. The mouse works in `htop`.

### Stage 7: GPU rendering

Do this stage only when stage 6 is complete. Measure the CPU renderer first, so you can compare.

Concepts:

- The GPU pipeline: vertex buffer, vertex shader, fragment shader, texture, bind group.
- Glyph atlas: rasterize each glyph one time into one large texture.
- Instanced drawing: one quad, drawn one time for each cell, with data for position, glyph, and color.
- Damage tracking: upload only the cells that changed.

Work:

1. Draw one colored triangle with `wgpu`.
2. Draw one textured quad.
3. Build the glyph atlas from your `fontdue` bitmaps.
4. Draw the full grid with one instanced draw call.

Test: `cat` a large file. Compare frame time and CPU use with the stage 4 renderer.

Read: "Learn Wgpu" (sotrh.github.io/learn-wgpu). Read the Ghostty and Alacritty renderer source after you finish, not before.

### Stage 8: Your features

Now you add the features that make it your terminal.

1. **Shell integration**: learn OSC 133 (marks `A`, `B`, `C`, `D`). Change your PowerShell `prompt` function to print the marks. Your terminal now knows where a prompt and a command start.
2. **Click-to-edit**: you know the command start cell and the cursor cell. Think about how to move the shell cursor to a clicked cell. Hint: the shell understands only keys.
3. **Restart**: stop the child process and start a new one. Find out why the new shell does not see a new `PATH`, and where Windows keeps the current value.
4. **On-demand tabs**: each tab has its own PTY, parser, and grid. The tab bar is a row of rectangles that you draw only if there are two or more tabs.
5. **Split panes**: use a binary tree. Each leaf is a terminal. Each node has a direction and a ratio.
6. **Regex search**: use the `regex` crate on the scrollback rows. Think about a match that goes across a wrapped line.

### Stage 9: Go deeper (optional)

- Replace `vte` with your own parser. Use the state diagram from stage 2.
- Replace `portable-pty` with direct ConPTY calls.
- Text shaping and ligatures (`rustybuzz`, `cosmic-text`).
- Double-width characters and emoji (`unicode-width`). Grapheme clusters.
- The Kitty keyboard protocol.
- Measure input latency. Read about "typometer".

## 4. How to debug a terminal

- Log each parser event to a file. Most display bugs are one wrong or missing sequence.
- Record the raw PTY bytes to a file. Replay the file into your parser in a unit test.
- Compare with a terminal that works. Run the same program in Windows Terminal.
- `vttest` shows which sequence is wrong.
- In WSL, `infocmp xterm-256color` shows what programs expect from you. Set `TERM=xterm-256color`.

## 5. Compare with the primary version

After each stage, read the same part of the primary version in `src/`.
Ask these questions:

- What did the library do that you wrote by hand?
- Which version is faster, and why?
- Which edge case did you miss?

## 6. Stage checklist

- [ ] 0 Rust base
- [ ] 1 PTY: raw bytes from `pwsh`
- [ ] 2 Parser: events printed
- [ ] 3 Grid: unit tests pass
- [ ] 4 Window: type a command, see the output
- [ ] 5 `vim` and `htop` work
- [ ] 6 Selection, clipboard, mouse
- [ ] 7 GPU renderer
- [ ] 8 Your features
- [ ] 9 Go deeper
