//! `fuse-ppkg`: mount a PS5 debug FPKG (`.pkg`) as a read-only directory.
//!
//! ```text
//! fuse-ppkg mount GAME.pkg /mnt/game [--passcode P] [--uid N] [--gid N]
//!                                 [--cache-mb N] [--readahead-mb N] [--fetch-kb N] [--max-read-kb N] [--workers N] [--format auto|pkg|ffpkg|exfat|ffpfs|ffpfsc|folder] [--auto-unmount]
//! fuse-ppkg list  GAME.pkg [--passcode P]
//! fuse-ppkg unmount /mnt/game
//! ```

use std::process::Command;

use fuse_ppkg::{Options, PkgFs};
use fuser::MountOption;

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("fuse-ppkg: {msg}");
    std::process::exit(1)
}

fn usage() -> ! {
    eprintln!(
        "usage:\n  \
         fuse-ppkg mount GAME.pkg MOUNTPOINT [--passcode P] [--uid N] [--gid N] [--cache-mb N] \
         [--readahead-mb N] [--auto-unmount]\n  \
         fuse-ppkg list GAME.pkg [--passcode P]\n  \
         fuse-ppkg unmount MOUNTPOINT"
    );
    std::process::exit(2)
}

struct Args {
    command: String,
    positional: Vec<String>,
    passcode: Option<String>,
    uid: u32,
    gid: u32,
    cache_mb: usize,
    readahead_mb: usize,
    fetch_kb: usize,
    max_read_kb: usize,
    workers: usize,
    format: fuse_ppkg::Format,
    auto_unmount: bool,
}

fn parse_args() -> Args {
    let mut args = Args {
        command: String::new(),
        positional: Vec::new(),
        passcode: None,
        uid: 0,
        gid: 0,
        cache_mb: 256,
        readahead_mb: 32,
        fetch_kb: 256,
        max_read_kb: 1024,
        workers: 4,
        format: fuse_ppkg::Format::Auto,
        auto_unmount: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "mount" | "list" | "unmount" if args.command.is_empty() => args.command = arg,
            "--passcode" => args.passcode = Some(it.next().unwrap_or_else(|| usage())),
            "--uid" => {
                args.uid = it
                    .next()
                    .unwrap_or_else(|| usage())
                    .parse()
                    .unwrap_or_else(|_| usage())
            }
            "--gid" => {
                args.gid = it
                    .next()
                    .unwrap_or_else(|| usage())
                    .parse()
                    .unwrap_or_else(|_| usage())
            }
            "--cache-mb" => {
                args.cache_mb = it
                    .next()
                    .unwrap_or_else(|| usage())
                    .parse()
                    .unwrap_or_else(|_| usage())
            }
            "--readahead-mb" => {
                args.readahead_mb = it
                    .next()
                    .unwrap_or_else(|| usage())
                    .parse()
                    .unwrap_or_else(|_| usage())
            }
            "--auto-unmount" => args.auto_unmount = true,
            "-h" | "--help" => usage(),
            _ => args.positional.push(arg),
        }
    }
    if args.command.is_empty() {
        usage();
    }
    args
}

fn options(a: &Args) -> Options {
    Options {
        passcode: a.passcode.clone(),
        uid: a.uid,
        gid: a.gid,
        cache_bytes: a.cache_mb << 20,
        readahead_bytes: a.readahead_mb << 20,
        fetch_bytes: ((a.fetch_kb.max(4)) << 10) as u64,
        workers: a.workers,
        format: a.format,
    }
}

fn main() {
    let args = parse_args();
    match args.command.as_str() {
        "mount" => mount(&args),
        "list" => list(&args),
        "unmount" => unmount(&args),
        _ => usage(),
    }
}

fn mount(a: &Args) {
    let (Some(pkg), Some(mountpoint)) = (a.positional.first(), a.positional.get(1)) else {
        usage()
    };
    let pkg = std::path::Path::new(pkg);
    let started = std::time::Instant::now();
    let state = fuse_ppkg::State::new(pkg, options(a))
        .unwrap_or_else(|e| die(format!("{}: {e}", pkg.display())));
    eprintln!(
        "fuse-ppkg: indexed {} entries in {:.1?}",
        state.len(),
        started.elapsed()
    );
    let mut opts = vec![
        MountOption::RO,
        MountOption::NoAtime,
        MountOption::NoExec,
        MountOption::NoSuid,
    ];
    // A filesystem name with no comma, so `mount` shows something readable.
    let name = pkg
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("fpkg")
        .replace(',', "_");
    opts.push(MountOption::FSName(format!("fuse-ppkg:{name}")));
    if a.max_read_kb > 0 {
        opts.push(MountOption::CUSTOM(format!(
            "max_read={}",
            a.max_read_kb << 10
        )));
    }
    if a.auto_unmount {
        opts.push(MountOption::AutoUnmount);
    }
    eprintln!("fuse-ppkg: mounting on {mountpoint} (read-only)");
    if let Err(e) = fuser::mount2(PkgFs(std::sync::Mutex::new(state)), mountpoint, &opts) {
        die(format!("mount failed: {e}"));
    }
}

fn list(a: &Args) {
    let Some(pkg) = a.positional.first() else {
        usage()
    };
    let opts = options(a);
    let state = fuse_ppkg::State::new(std::path::Path::new(pkg), opts)
        .unwrap_or_else(|e| die(format!("{pkg}: {e}")));
    let mut paths: Vec<&str> = state.paths();
    paths.sort_unstable();
    let total: u64 = paths.iter().map(|p| state.size_of_path(p)).sum();
    for p in paths {
        println!("{:>12}  {p}", state.size_of_path(p));
    }
    eprintln!("fuse-ppkg: {} entries, {total} bytes", state.len());
}

fn unmount(a: &Args) {
    let Some(mountpoint) = a.positional.first() else {
        usage()
    };
    for bin in ["fusermount3", "fusermount"] {
        if Command::new(bin)
            .arg("-u")
            .arg("-z")
            .arg("--")
            .arg(mountpoint)
            .status()
            .is_ok_and(|s| s.success())
        {
            return;
        }
    }
    // A mount done by root's mount syscall (no fusermount) falls back to umount.
    if Command::new("umount")
        .arg(mountpoint)
        .status()
        .is_ok_and(|s| s.success())
    {
        return;
    }
    die(format!("could not unmount {mountpoint}"));
}
