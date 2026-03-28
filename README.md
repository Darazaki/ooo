# `ooo`

Tool to interact with ooo archives

## What is an `ooo` archive?

It's an easily concatenable archive format that allows archives to be combined together just by doing `cat archive1.ooo archive2.ooo > archive1+2.ooo`

It saves the following information about each entry in an ASCII readable way:

- filename
- type (file or symlink => empty directories aren't supported)
- compressed size
- mode (unix-style basic rwx*3 permissions)
- compression filter type
- uncompressed file crc32 for validation

Each entry is independent from each other

The following compression methods (or "filters") are supported: zstd, lzma, lz4, and flate

## Install

```sh
cargo install --git https://github.com/Darazaki/sldd
```

After that, `ooo` should be in your `$PATH`

## Usage

```
Tool to interact with ooo archives

Usage: ooo [OPTIONS] <COMMAND>

Commands:
  add      Add files to an archive [aliases: a]
  extract  Extract files from an archive [aliases: x]
  list     List files contained in an archive [aliases: l]
  help     Print this message or the help of the given subcommand(s)

Options:
  -v, --verbose  
  -h, --help     Print help
  -V, --version  Print version
```
