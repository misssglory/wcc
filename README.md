# wcc

`wcc` is a Rust command wrapper that runs arbitrary commands, streams stdout/stderr live, stores command history, keeps rolling output tails according to configurable limits, compresses large stored tails, and copies the final clipboard payload in this format:

```text
$ command args

[stdout]
...

[stderr]
...
```

It also includes `wcc gui -- ...` for a ratatui-based terminal UI showing live stats and history.

## Features

- Run any command through `wcc -- command args`
- Live passthrough of stdout/stderr while the child process runs
- Clipboard update on completion or when interrupted
- Persistent JSON history under `~/.local/state/wcc/history`
- Configurable retention policy by `lines`, `words`, `chars`, or `bytes`
- Compression of large stored tails with gzip+base64
- Ratatui UI with:
  - live elapsed time
  - live stdout/stderr line/word/char/byte counters
  - recent history browser
  - copy selected entry back to clipboard
  - delete selected history entry

## CLI usage

```bash
cargo run -- -- bash -lc 'echo hello; echo err >&2'
cargo run -- gui -- bash -lc 'for i in {1..5}; do echo out:$i; echo err:$i >&2; sleep 1; done'
```

## Config

Default config path:

```text
~/.config/wcc/config.toml
```

Example:

```toml
history_dir = "/home/user/.local/state/wcc/history"
compress_above_bytes = 16384

[retain]
mode = "bytes"
limit = 131072
```

`retain.mode` may be `lines`, `words`, `chars`, or `bytes`.

## Notes

- `wcc --gui` from the original request is implemented as `wcc gui -- ...` because clap subcommands are more idiomatic and easier to maintain. If you want, this can be changed to parse a literal `--gui` flag instead.
- The current history format stores rolling tails plus optional compressed copies of those same tails when they cross the compression threshold. If you want full-output archival before trimming, the code should be extended to spool full streams to temp files and compress those for history.
- In plain CLI mode, live stats are computed internally but only the child output is printed immediately; final clipboard content intentionally excludes stats.

## Build

```bash
cargo build --release
```
