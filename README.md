# shellsmith

[![CI](https://github.com/konomanoasa/shellsmith/actions/workflows/ci.yaml/badge.svg)](https://github.com/konomanoasa/shellsmith/actions/workflows/ci.yaml)
[![crates.io](https://img.shields.io/crates/v/konomanoasa-shellsmith.svg)](https://crates.io/crates/konomanoasa-shellsmith)

Command-line interface and language server for shell scripts.

## Supported shells

- POSIX sh

## Features

- Document links
- Document symbols
- Find references
- Formatting
- Go to definition
- Rename
- Semantic tokens
- Source following
- Workspace symbols

Workspace indexing respects `.gitignore`.

See the [roadmap](ROADMAP.md) for planned features.

## Requirements

- C compiler
- macOS or Linux
- Rust 1.95 or later

## Installation

```sh
cargo install konomanoasa-shellsmith --locked
```

## Usage

Format shell files in place, respecting `.gitignore` when scanning directories.

```sh
shellsmith format script.sh
shellsmith format .
```

Start the LSP server.

```sh
shellsmith lsp
```

### Source annotations

For unresolved sources, use an absolute path or one relative to the annotated file.

```sh
# shellsmith source="./lib.sh"
. "$(dirname "$0")/lib.sh"
```

## License

[MIT](LICENSE)
