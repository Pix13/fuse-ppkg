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
fuse-ppkg mount GAME.pkg MOUNTPOINT [--passcode P] [--uid N] [--gid N]
                                    [--cache-mb N] [--readahead-mb N] [--auto-unmount]
fuse-ppkg list GAME.pkg [--passcode P]
fuse-ppkg unmount MOUNTPOINT
```

| Option | Default | Meaning |
|---|---|---|
| `--passcode` | debug default (`0000…`) | Passcode of a native (AES-XTS) image |
| `--uid`, `--gid` | `0` | Owner shown for every entry |
| `--cache-mb` | `32` | Decoded data kept in memory |
| `--readahead-mb` | `4` | Read ahead on sequential access (`0` to disable) |
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
- One `Mutex` serializes reads: a multi-threaded FUSE client queues behind it.
- The tree is walked once at mount time (476 ms for 40 000 entries here); very large titles cost
  seconds and some hundreds of MB of index memory.
- macOS needs macFUSE and Windows WinFsp; neither is wired here.

## Performance (this machine, synthetic package)

| Workload | Result |
|---|---|
| Sequential read (`dd`, `O_DIRECT`) | 142 MB/s |
| 4 KiB random read, 32 MB cache | p50 1.9 ms |
| 4 KiB random read, cache ≥ file size | p50 0.13 ms |
| Mount (index 40 208 entries) | 476 ms, 3.6 MiB RSS |

Random reads are the weak spot: a 4 KiB read costs a whole 256 KiB block decode on a miss.
Size `--cache-mb` to the files a game streams and the cost disappears.

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
