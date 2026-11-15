# rsedit

A fast, lightweight code editor for Linux, written in Rust.

rsedit gives you the parts of a modern editor you use every day (tabs and
splits, a file tree, quick open, find in files, git and language servers)
without the weight. It starts quickly, draws its UI on the CPU instead of
loading a GPU driver, and an idle window uses around 20 MiB of memory.

## Features

**Editing**
- Multiple cursors: add the next occurrence, select all occurrences
- Move, copy, delete and insert lines; toggle comments; indent and outdent
- Bracket matching, indentation guides, highlighting of other occurrences
- Syntax highlighting for the common languages (syntect grammars)
- Undo history that records only what changed, and stays small for large files
- LF and CRLF files, converted on load and save; tabs or spaces per file
- Find and replace in the file, find in files across the project

**Navigating**
- Quick open (`Ctrl+P`), command palette (`Ctrl+Shift+P`), go to line (`Ctrl+G`)
- File tree with create, rename, delete and reveal
- Tabs in side-by-side groups; drag tabs to split or move them
- Back and forward through where you've been
- The open folder, tabs and cursor positions are restored on the next start

**Language servers** (when installed: `rust-analyzer` for Rust, `clangd` for C and C++)
- Completion, with words from the file and language keywords as fallback
- Hover docs, parameter hints while typing a call
- Go to definition, find all references, rename symbol
- Errors and warnings inline and in a Problems panel
- Language servers only run in folders you trust: they run the project's code
  (rust-analyzer builds it, including build scripts and macros), so rsedit asks
  once per folder and remembers the answer in `~/.config/rsedit/trusted-folders`
- C/C++ projects without `compile_commands.json` still work: rsedit guesses
  include paths (from `.vscode/c_cpp_properties.json` or by scanning the
  project) and hands them to clangd without writing anything into the project

**Formatting**
- Format Document and optional format on save
- Uses the language server, or any command you configure per language
  (text in on stdin, formatted text out on stdout)

**Git**
- Changed lines marked next to the line numbers, and a diff view per file
- Stage, unstage and discard changes; commit
- Switch branches; pull, push and fetch

**And also**
- Markdown preview
- A notes file one shortcut away (`Ctrl+Shift+N`)
- Light and dark theme, zoom

## Install

rsedit runs on Linux, on Wayland and X11.

You need a Rust toolchain (1.88 or newer) and a C compiler (the syntax
highlighter uses the Oniguruma regex library). Then:

```sh
git clone https://github.com/Ceddi147/rsedit.git
cd rsedit
./install.sh
```

This builds a release binary into `~/.cargo/bin/rsedit` and adds rsedit to your
application launcher. No root needed. `./install.sh --uninstall` removes it again.

To just run it from the checkout:

```sh
cargo run --release -- path/to/file-or-folder
```

For language features, install the servers you need (`rust-analyzer`,
`clangd`) so they are on your `PATH`.

## Usage

```sh
rsedit                 # restore the last session
rsedit ~/code/project  # open a folder
rsedit notes.md        # open a file (and its folder)
```

Every command is in the command palette (`Ctrl+Shift+P`). Some shortcuts:

| Shortcut | Command |
|---|---|
| `Ctrl+P` | Go to file |
| `Ctrl+Shift+P` | Command palette |
| `Ctrl+Shift+F` | Find in files |
| `Ctrl+B` | Toggle sidebar |
| `Ctrl+\` | Split right |
| `F12` / `Shift+F12` | Go to definition / find references |
| `F2` | Rename symbol |
| `Alt+Shift+F` | Format document |
| `Ctrl+Shift+G` | Source control |

## Settings

"Preferences: Open Settings" opens `~/.config/rsedit/settings.json` in a tab;
changes apply when you save it.

```json
{
    "notes.file": "~/notes.md",
    "notes.shortcut": "Ctrl+Shift+N",
    "clangd.backgroundIndex": false,
    "format.onSave": false,
    "format.commands": {
        "cpp": ["clang-format"],
        "rs": ["rustfmt", "--edition", "2024"]
    }
}
```

`format.commands` maps a file extension or language to a formatter command.
An empty command turns formatting off for that language.

## Why it's light

- **No GPU.** The UI ([egui](https://github.com/emilk/egui)) is rasterized
  on the CPU straight into the window's shared-memory buffer. Loading a GPU
  driver alone would cost more memory than the whole editor.
- **Only what's visible.** The editor lays out and paints only the lines on
  screen, so files can be large.
- **Nothing in the background unless asked.** Indexing for quick open runs only
  while the palette is open; search and git run on background threads so the
  UI never waits.
- **Few dependencies, trimmed features.** For example, no image loading in
  the Markdown preview and no system font scanning.

## How it's made

rsedit is vibe coded. Nearly all of the code was written by an AI coding
assistant ([Claude Code](https://claude.com/claude-code)), steered by
conversation rather than typed by hand.

That doesn't mean anything goes. I'm a software engineer, and I know what
good software needs, so that's what I look out for: behavior covered by tests,
performance and memory measured rather than guessed, dependencies that earn
their place, and code that stays readable. The AI writes the code; deciding
what good looks like, and checking that rsedit meets it, stays my job.

## Status

A personal project for Linux. Expect rough edges, and feel free to open an
issue.
