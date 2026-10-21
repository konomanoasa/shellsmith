# shellsmith

[![CI](https://github.com/konomanoasa/shellsmith/actions/workflows/ci.yaml/badge.svg)](https://github.com/konomanoasa/shellsmith/actions/workflows/ci.yaml)
[![crates.io](https://img.shields.io/crates/v/konomanoasa-shellsmith.svg)](https://crates.io/crates/konomanoasa-shellsmith)

Language server for shell scripts.

## Supported shells

- POSIX sh

## Features

- Document links
- Document symbols
- Find references
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

Start the LSP server.

```sh
shellsmith lsp
```

### Source annotations

Annotate unresolved source paths.

```sh
# shellsmith source="./lib.sh"
. "$(dirname "$0")/lib.sh"
```

Automatic resolution takes precedence. Annotation paths are absolute or relative
to the directory of the file containing the annotation.

## License

[MIT](LICENSE)
