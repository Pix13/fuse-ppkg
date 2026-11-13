#!/bin/sh
# End-to-end smoke test: build a package, mount it, compare it with the CLI's own extraction.
# Needs /dev/fuse, fusermount3 (or fusermount) and a forge checkout for the extraction.
set -eu
FORGE=${FORGE:-../ps5-dump-forge}
WORK=$(mktemp -d)
trap 'fusermount3 -u -z "$WORK/mnt" 2>/dev/null || fusermount -u -z "$WORK/mnt" 2>/dev/null || true; rm -rf "$WORK"' EXIT INT TERM

cargo run --release --example mkpkg -- "$WORK/game.pkg" 300 1048576 >/dev/null
mkdir "$WORK/mnt"
cargo run --release -- mount "$WORK/game.pkg" "$WORK/mnt" --cache-mb 8 &
MOUNT_PID=$!
for _ in $(seq 50); do mountpoint -q "$WORK/mnt" && break; sleep 0.1; done

count=$(find "$WORK/mnt" -type f | wc -l)
want=$(./target/release/fuse-ppkg list "$WORK/game.pkg" 2>/dev/null | wc -l)
[ "$count" -eq "$want" ] || { echo "FAIL: $count files mounted, $want listed by the reader"; exit 1; }
grep -q PPSA01234 "$WORK/mnt/sce_sys/param.json" || { echo "FAIL: param.json"; exit 1; }
[ "$(stat -c %s "$WORK/mnt/data/big0.bin")" -eq 1048576 ] || { echo "FAIL: big.bin size"; exit 1; }

if [ -d "$FORGE" ]; then
  cargo run --release --manifest-path "$FORGE/Cargo.toml" -p ps5-dump-forge-cli -- \
    convert "$WORK/game.pkg" --to folder -o "$WORK/extracted" >/dev/null
  diff -r "$WORK/mnt" "$WORK/extracted" || { echo "FAIL: mounted tree differs from the extraction"; exit 1; }
  # Concurrent readers, four at a time, every byte hashed.
  (cd "$WORK/mnt" && find . -type f -print0 | xargs -0 -P4 sha256sum) > "$WORK/a.sum"
  (cd "$WORK/extracted" && find . -type f -print0 | xargs -0 -P4 sha256sum) > "$WORK/b.sum"
  sort -o "$WORK/a.sum" "$WORK/a.sum" && sort -o "$WORK/b.sum" "$WORK/b.sum"
  cmp "$WORK/a.sum" "$WORK/b.sum" || { echo "FAIL: concurrent reads differ"; exit 1; }
fi

fusermount3 -u "$WORK/mnt" 2>/dev/null || fusermount -u "$WORK/mnt"
wait "$MOUNT_PID" 2>/dev/null || true
echo "OK: mounted, read and compared"
