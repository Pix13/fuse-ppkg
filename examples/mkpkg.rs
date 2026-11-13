//! Build a debug FPKG to test the filesystem with: `mkpkg OUT.pkg [FILES] [BIG_BYTES] [BIG_COUNT]`
//!
//! The tree is synthetic: `eboot.bin`, `sce_sys/param.json`, `FILES` small files under
//! `data/dir###/`, an empty directory and one random `BIG_BYTES` file. Built through
//! ps5-dump-forge's vendored builder, so the package is exactly what the reader expects.
use ps5upload_fpkg::build::{self, BuildControl, BuildRequest};
use ps5upload_fpkg::source::FolderSource;

const CID: &str = "UP0000-PPSA01234_00-TESTGAME00000000";

fn main() {
    let mut args = std::env::args().skip(1);
    let out = std::path::PathBuf::from(args.next().expect("out path"));
    let files: usize = args
        .next()
        .unwrap_or_else(|| "500".into())
        .parse()
        .expect("files");
    let big: usize = args
        .next()
        .unwrap_or_else(|| "8388608".into())
        .parse()
        .expect("big bytes");
    let big_count: usize = args
        .next()
        .unwrap_or_else(|| "1".into())
        .parse()
        .expect("big count");
    let src = std::env::temp_dir().join(format!("fuse-ppkg-mkpkg-{}", std::process::id()));
    std::fs::create_dir_all(src.join("sce_sys")).unwrap();
    let mut rng = 0x1234_5678_9abc_def0u64;
    let mut bytes = |n: usize| -> Vec<u8> {
        (0..n)
            .map(|_| {
                rng ^= rng >> 12;
                rng ^= rng << 25;
                rng ^= rng >> 27;
                (rng.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) as u8
            })
            .collect()
    };
    std::fs::write(
        src.join("sce_sys/param.json"),
        format!("{{\"contentId\":\"{CID}\",\"contentVersion\":\"01.001.000\",\"titleId\":\"PPSA01234\"}}"),
    )
    .unwrap();
    std::fs::write(src.join("eboot.bin"), bytes(90_000)).unwrap();
    for i in 0..files {
        let p = src.join(format!("data/dir{:02}/f{i:05}.bin", i % 40));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, bytes(2_000 + (i * 977) % 40_000)).unwrap();
    }
    std::fs::create_dir_all(src.join("data/empty")).unwrap();
    for i in 0..big_count {
        std::fs::write(src.join(format!("data/big{i}.bin")), bytes(big)).unwrap();
    }
    let outdir = out.parent().expect("out path has a parent");
    let mut r = BuildRequest::production(&src, outdir);
    r.time = Some((1_700_000_000, 0));
    r.threads = Some(std::thread::available_parallelism().map_or(1, |n| n.get()));
    r.level = ps5upload_fpkg::kraken::Level::Fast;
    let mut tree = FolderSource::open(&src).unwrap();
    let prepared = build::prepare(&mut tree, &r, &mut BuildControl::default()).unwrap();
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&out)
        .unwrap();
    build::write_package(
        &prepared,
        &mut tree,
        &mut file,
        &out,
        &mut |_| {},
        &mut BuildControl::default(),
    )
    .unwrap();
    println!("{}", std::fs::metadata(&out).unwrap().len());
    std::fs::remove_dir_all(&src).ok();
}
