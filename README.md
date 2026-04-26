# wcc - Workflow Clipboard Commander

A collection of command-line tools for clipboard operations, code analysis, and development workflow automation.

## Tools Overview

| Tool | Purpose | Description |
|------|---------|-------------|
| `wcn` | Copy to clipboard | Copy file content, functions, or specific lines to clipboard |
| `wcp` | Paste from clipboard | Write clipboard content to files with backup and diff |
| `wcf` | Function replacement | Replace function implementations across Rust files |
| `wcl` | List/Analyze | Analyze directories and copy file contents |
| `wcc` | Command wrapper | Run commands and capture stdout/stderr to clipboard |

## Installation

```bash
cargo install --path .

# Or build specific binaries:
cargo build --release --bin wcn
cargo build --release --bin wcp
cargo build --release --bin wcf
cargo build --release --bin wcl
cargo build --release --bin wcc
```

## Configuration
All tools use a unified configuration file at ~/.config/wcc/config.toml:

```toml
[wcc]
history_dir = "/home/user/.local/state/wcc/history"
compress_above_bytes = 16384
time_format = "%H:%M:%S %d.%m.%Y"
default_cargo_mode = "debug"

[wcl]
max_file_size_kb = 50
max_file_words_to_copy = 10000
skip_patterns = [".o", ".pyc", ".exe", ".bkp"]
skip_dirs = ["target", "node_modules", ".git"]
show_function_details = true
parallel_processing = true
max_threads = 8

[wcn]
show_time_in_header = true

[wcp]
auto_backup = true
backup_suffix = ".bkp"

[wcf]
auto_format = true
backup_before_replace = true
```


## Tool Details
### wcn - Copy to Clipboard
Copy file content to clipboard, with optional filtering and function extraction.

```bash
# Copy entire file
wcn src/main.rs

# Copy first/last N lines
wcn -h 10 src/main.rs      # first 10 lines
wcn -t 20 src/main.rs      # last 20 lines
wcn -h 10 -t 10 file.rs    # show head and tail

# Extract specific function
wcn -f main src/main.rs

# Interactive file selection with fzf
wcn

# With flags
wcn -h 30 -f parse_args src/main.rs
```

### Options:

- -h, --head N - Copy first N lines

- -t, --tail N - Copy last N lines

- -f, --function NAME - Extract a specific function

### Features:

- Adds comment header with filename and timestamp

- Respects language-specific comment syntax

- Shows colored statistics

## wcp - Paste from Clipboard
Write clipboard content to files with automatic backups and diff visualization.

```bash
# Write to specific file
wcp output.txt

# Write to path (creates directories if needed)
wcp src/new_file.rs

# Interactive with fzf
wcp
```

### Features:

- Creates backup (file.bkp) before overwriting

- Shows colored diff between old and new content

- Per-function diff statistics

- Creates parent directories automatically

- fzf file picker for interactive selection

## wcf - Function Replacement
Replace function implementations across Rust files using clipboard content.

```bash
# Replace in entire directory
wcf src/

# Replace in specific file
wcf src/main.rs

# Interactive mode with fzf selection
wcf
```

### Features:

- Multi-function clipboard parsing

- Preserves function signatures (pub, async, etc.)

- Runs rustfmt after replacement

- Creates backups before modifications

- Interactive confirmation per function

- Colored diff preview

- Accumulates all changes to clipboard

## wcl - Directory Analyzer
Analyze directories and copy file contents to clipboard.

```bash
# Analyze current directory
wcl

# Analyze specific path
wcl /path/to/project
```

### Output:

- Summary statistics (files, lines, words, chars, bytes)

- Per-file breakdown with function/class detection

- Function/class details with line numbers

- Usage statistics (optional)

- Full file contents for small files (configurable)

- Configuration:

- max_file_words_to_copy - Maximum words to include (default: 10000)

- skip_patterns - File extensions to skip

- skip_dirs - Directories to skip

- parallel_processing - Enable multi-threading

## wcc - Command Wrapper
Run commands and capture stdout/stderr to clipboard.

```bash
# Run command and capture output
wcc -- cargo build
wcc -- ls -la

# Build wrapper (cargo build)
wcc build
wcc build --release

# Run wrapper (cargo run)
wcc run
wcc run --release

# Configure default mode
wcc config --show
wcc config --set-cargo-mode release
```

### Features:

- Captures both stdout and stderr

- Preserves colors in terminal, strips for clipboard

- Saves command history with timestamps

- Configurable default build mode

- Shows diff statistics for changes

## Shared Features
### Colored Output
- All tools support beautiful colored output with:

- Heatmap colors for statistics (based on magnitude)

- Syntax highlighting for diffs

- Color-coded file names and function names

### fzf Integration
Tools like `wcn`, `wcp`, and `wcf` support fzf for interactive file selection:

### File preview with bat or head

- Typing new paths supported

- Auto-completion of existing files

### Clipboard Integration
All tools automatically copy relevant content to clipboard:

- `wcn` -> File content

- `wcp` -> Diff summary

- `wcf` -> Function replacement diff

- `wcl` -> Analysis report + file contents

- `wcc` -> Command output

### Unified Configuration
Single config file for all tools with sensible defaults and sections for each tool.

## Examples
### Copy function and replace in another file
```bash
# Copy function from source
wcn -f process_data src/module.rs

# Replace in target file
wcf src/target.rs
```

### Analyze project and copy for documentation

```bash
# Analyze and copy all small files
wcl src/

# Clipboard now contains:
# - Analysis report
# - All file contents (under 10000 words)
```

### Backup before editing
```bash
# wcp automatically creates .bkp files
wcp config.toml

# Edit file
vim config.toml

# Restore if needed
cp config.toml.bkp config.toml
```

## Development
### Building
```bash
# Build all
cargo build --release

# Build specific tool
cargo build --release --bin wcn
```

## Adding New Features
The tools share common modules:

- `common.rs` - Shared utilities (clipboard, colors, stats)

- `lib.rs` - Unified configuration handling

## Dependencies
### Essential:

- anyhow - Error handling

- arboard - Clipboard operations

- regex - Pattern matching

- similar - Diff generation

- serde - Configuration serialization

### Optional:

- `rayon` - Parallel processing (wcl)

- `crossterm` - Terminal colors

- `fzf` - Interactive file selection (external)

## License
MIT
