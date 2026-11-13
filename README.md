# fuse-ppkg

Mount a PS5 debug FPKG (`.pkg`) as a read-only directory. Nothing is unpacked: file bytes are
read at their offsets in the `.pkg` and Kraken blocks are decoded only where a read touches
them, through [ps5-dump-forge](https://github.com/quer3q/ps5-dump-forge)'s `FpkgSource`.

```console
$ fuse-ppkg mount PPSA01234.pkg /mnt/ps5
fuse-ppkg: indexed 40208 entries in 476ms
fuse-ppkg: mounting on /mnt/ps5 (read-only)
$ ls /mnt/ps5
eboot.bin  sce_sys  data
$ sha256sum /mnt/ps5/data/big.bin
dac08989b51f74570224ac52b9296db52a9cb335bfc3b7f3d9680f91526e5e55  /mnt/ps5/data/big.bin
$ fuse-ppkg unmount /mnt/ps5
```

## Status

Prototype, read-only, Linux. It works on packages this reader supports, which is the point of
the exercise — see **Limits** below before pointing it at your library.

## Requirements

- Linux with `/dev/fuse` and `fusermount3` (or `fusermount`); unprivileged mounts are fine.
- Rust 1.85+ to build.

## Usage

```
fuse-ppkg mount GAME.pkg MOUNTPOINT [--passcode P] [--uid N] [--gid N] [--workers N]
                                    [--cache-mb N] [--readahead-mb N] [--fetch-kb N]
                                    [--max-read-kb N] [--auto-unmount]
fuse-ppkg list GAME.pkg [--passcode P]
fuse-ppkg unmount MOUNTPOINT
```

| Option | Default | Meaning |
|---|---|---|
| `--passcode` | debug default (`0000…`) | Passcode of a native (AES-XTS) image |
| `--uid`, `--gid` | `0` | Owner shown for every entry |
| `--workers` | `4` | Decoding workers, one reader each; files are sharded over them |
| `--cache-mb` | `256` | Decoded data kept in memory, split over the workers |
| `--readahead-mb` | `32` | Read ahead on sequential access (`0` to disable) |
| `--fetch-kb` | `256` | Bytes fetched per miss (one Kraken block) |
| `--max-read-kb` | `1024` | Largest read the kernel may send |
| `--auto-unmount` | off | Unmount when the process exits |

`list` prints the package's tree without mounting it (same reader, no FUSE).

## Limits

- **Debug FPKG only.** Retail packages are refused by the reader (their header is signed); a
  debug package built with a non-default passcode needs `--passcode`.
- **Read-only.** The format lists regular files and directories only, with no mode, owner or
  timestamp: modes are synthesized (`0555`/`0444`) and the package's mtime is used. There are no
  symlinks, no xattrs, no write path.
- **Patch packages** (`.pkg` update with a `\x7FLIH` header) list only the files they carry whole;
  blocks of the base game are not served, so mount the base package for those.
- Only Linux: macOS (macFUSE) and Windows (WinFsp) are not wired.
- Memory: `--cache-mb` is the whole budget, divided by `--workers`. Each worker also opens the
  package and walks its tree once, so `--workers` costs mount time and index memory.
- The tree is walked at mount time, once per worker (476 ms for 40 000 entries here); very large
  titles cost seconds and some hundreds of MB of index memory.

## Performance (this machine, synthetic package)

Measured on a 560 MB package (four 128 MB files), defaults, `dd ... iflag=direct`:

| Workload | Result |
|---|---|
| Sequential read, one stream | 175 MB/s (210 peak) |
| Four concurrent streams | 385 MB/s aggregate (500 peak) |
| 4 KiB random read | p50 0.18 ms |
| 64 KiB reads over many small files | 0.14 ms |
| Mount (index 40 208 entries) | 476 ms |

For comparison, the first synchronous version of this adapter did 91 MB/s sequential,
2.2 ms per 4 KiB random read and 17 MB/s over four streams. What changed:

- decoding moved to a worker thread, so FUSE callbacks serve cache hits and only wait for a
  chunk that is already being fetched;
- one reader and cache per worker, files sharded over them by path hash, so separate files
  decode side by side;
- read-ahead windows (32 MB) scheduled ahead of a sequential read, fetched as merged 8 MB
  ranges so the decoder runs wide on every core;
- chunks shared as `Arc<[u8]>`, so a request copies only its own bytes.

## Build and test

```console
$ cargo build --release
$ cargo test
$ FORGE=../ps5-dump-forge scripts/smoke.sh   # builds a package, mounts it, diffs it
```

`scripts/smoke.sh` builds a synthetic package with `examples/mkpkg`, mounts it, compares every
byte with the same package extracted by the forge CLI, and unmounts.

## Thanks

The whole format work is [ps5-dump-forge](https://github.com/quer3q/ps5-dump-forge)'s
(GPL-3.0-or-later): its `FpkgSource` reader, and ps5upload's engine it vendors. This crate is
only the FUSE adapter. The feasibility study and its measurements are in
[docs/STUDY.md](docs/STUDY.md).

## License

MIT for this crate (see `LICENSE`); the linked forge crates are GPL-3.0-or-later, so a
distributed binary is GPL-3.0-or-later (see `NOTICE`).
