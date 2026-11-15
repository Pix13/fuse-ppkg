# fuse-ppkg

Mount a PS5 game source — debug FPKG (`.pkg`), `.ffpkg` (UFS2), `.exfat`, `.ffpfs` (PFS),
`.ffpfsc` (compressed PFS) or a game folder — as a read-only directory. Nothing is unpacked:
file bytes are read at their offsets in the image and only the blocks a read touches are
decoded, through [ps5-dump-forge](https://github.com/quer3q/ps5-dump-forge)'s readers.

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

Prototype, read-only, Linux, tagged `v0.1`. It works on packages this reader supports — see
**Limits** below before pointing it at your library.

## Requirements

- Linux with `/dev/fuse` and `fusermount3` (or `fusermount`); unprivileged mounts are fine.
- The mount helper comes from the `fuse3` package of your distribution:

  | Distribution | Install |
  |---|---|
  | CachyOS, Arch | `sudo pacman -S --needed fuse3` |
  | Bazzite | shipped in the image — nothing to install |
  | Fedora Atomic (other images) | `rpm-ostree install fuse3` then reboot |
  | Fedora | `sudo dnf install fuse3` |
  | Debian, Ubuntu | `sudo apt install fuse3` |
  | Alpine | `sudo apk add fuse3` |
  | openSUSE | `sudo zypper install fuse3` |

  On other atomic images, check first (`rpm -q fuse3`): nothing to layer when it is already
  there. Layering survives image upgrades.
- Rust 1.85+ to build.

## Prebuilt binary

The [v0.1 release](https://github.com/Pix13/fuse-ppkg/releases/tag/v0.2) carries
`fuse-ppkg-x86_64-linux-musl`, a stripped static-pie x86-64 build: it depends on no libc, so
the distribution and its glibc version do not matter. The `fuse3` package is still needed at
run time (the mount helper), and the kernel's `fuse` module with `/dev/fuse`.

```console
$ curl -LO https://github.com/Pix13/fuse-ppkg/releases/download/v0.2/fuse-ppkg-x86_64-linux-musl
$ curl -LO https://github.com/Pix13/fuse-ppkg/releases/download/v0.2/fuse-ppkg-x86_64-linux-musl.sha256
$ sha256sum -c fuse-ppkg-x86_64-linux-musl.sha256
$ install -m 0755 fuse-ppkg-x86_64-linux-musl ~/.local/bin/fuse-ppkg
```

Mounting a real package with this binary was tested on Debian 12, Ubuntu 24.04, CachyOS (after
`pacman -S fuse3`), Bazzite (out of the box), Fedora 41, Alpine 3.20 and Alpine 3.21; every
distribution returned the same SHA-256 for the same file. Build or refresh it with:

```console
$ rustup target add x86_64-unknown-linux-musl
$ cargo build --release --target x86_64-unknown-linux-musl
```

## Usage

```
fuse-ppkg mount SOURCE MOUNTPOINT [--format auto|pkg|ffpkg|exfat|ffpfs|ffpfsc|folder]
                                    [--passcode P] [--uid N] [--gid N] [--workers N]
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
| `--format` | `auto` | Source format; `auto` reads the extension, else the first reader that opens it |
| `--auto-unmount` | off | Unmount when the process exits |

`list` prints the package's tree without mounting it (same reader, no FUSE).

## Limits

- **Supported sources**: `.pkg` (debug FPKG only — retail packages are refused by the reader,
  a debug one built with a non-default passcode needs `--passcode`), `.ffpkg`, `.exfat`,
  `.ffpfs` (unencrypted), `.ffpfsc` and folders.
- `.ffpfsc` decompresses zlib per 64 KiB block, so random reads cost more than on a plain
  image; give it a larger `--cache-mb` and expect lower throughput.
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

### On a real title (Astro Bot, 37.4 GB package)

The package holds 166 719 files (159.8 GB of logical content) and is Kraken-compressed
throughout, so it decodes slower than the synthetic one.

```console
$ fuse-ppkg mount EP9000-PPSA21567_00-….pkg /mnt/astro \
    --cache-mb 1024 --readahead-mb 64 --workers 4
fuse-ppkg: indexed 168865 entries in 2.7s
```

| Workload | Result |
|---|---|
| First pass, 583 MB video file | 50 MB/s |
| Second pass, same file | 92 MB/s |
| 4 KiB random read | p50 2.6 ms |
| Mount | 2.7 s, 361 MB RSS before the cache fills |

- `--cache-mb 1024` and `--readahead-mb 64` suit its large media files; `--workers 8` helps
  when several files play at once and the machine has the RAM.
- `--fetch-kb` makes no visible difference on a single large file here (256 to 4096 measured
  within noise); keep the 256 default for random access.
- Bytes match the package exactly: SHA3-256 of `eboot.bin` (259 MB) and `sce_sys/param.json`
  read through the mount equals the reader's own read.
- Start it detached, or pass `--auto-unmount`: killed with its shell, a foreground mount is
  left as "Transport endpoint is not connected".

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
