# rustCopyCardApp

Camera card copy tool, binary name `rcca`. The copy is delegated to the embedded
[sy](https://github.com/nijaru/sy) v0.4.1 (mirror sync); traversal and the XXH64
verification of both sides are done by this tool.

## Build

```bash
make dist               # release build: wipe dist/, build both platforms, write checksums
cargo build --release   # current platform only, binary at target/release/rcca
```

sy is vendored as source in `vendor/sy` (v0.4.1, with four small patches documented in
`vendor/sy/PATCHES.md`), so building needs neither network access nor a separate sy
install.

`make dist` produces nine artifacts:

- `dist/rcca-macos-arm64`: macOS arm64.
- `dist/rcca-windows-x64.exe`: Windows x64 (PE32+, system DLLs only).
- `dist/rcca-linux-x64`: Linux x86-64, static musl.
- `dist/rcca-linux-armv7`: Linux ARMv7 hard-float (Raspberry Pi 2/3 in 32-bit mode), static musl.
- `dist/rcca-linux-armv8`: Linux ARMv8 / aarch64, static musl.
- `dist/rcca-windows-x64-upx.exe`, `dist/rcca-linux-x64-upx`, `dist/rcca-linux-armv7-upx`,
  `dist/rcca-linux-armv8-upx`: the same four binaries packed with UPX, about a quarter of
  the size and slower to start.
- `dist/SHA256SUMS`: SHA-256 of all nine, verify with `cd dist && shasum -a 256 -c SHA256SUMS`.

The macOS binary is not packed: UPX refuses Mach-O arm64 unless `--force-macos` is given,
which upstream marks as unsupported. The unpacked Windows and Linux binaries are kept
next to the packed ones, so both are shipped. `make dist` needs `brew install upx` and
stops if the binary is missing; the level and flags are `UPX_FLAGS` (`-9 --lzma
--no-progress` by default), e.g. `make dist UPX_FLAGS='-1'` for a faster pack.

The three Linux binaries are fully static (no glibc, no OpenSSL shared library), so they
run on any distribution. Cross toolchains needed on macOS:

```bash
rustup target add x86_64-unknown-linux-musl armv7-unknown-linux-musleabihf aarch64-unknown-linux-musl
rustup target add x86_64-pc-windows-gnu     # Windows, plus: brew install mingw-w64
brew install zig                            # C compiler/sysroot for the musl targets
```

`scripts/zig-cc.sh` and `scripts/zig-ar.sh` wrap `zig` as the cross C compiler;
`rust-lld` from the toolchain does the linking. OpenSSL is built from source for the
Linux targets (`vendor`-free, see `Cargo.toml`), so the first `make dist` after a clean
takes a few minutes. Every run wipes `dist/` first, so no stale artifact survives.

Other entry points: `make test` runs every test, `make lint` runs clippy plus the
format check, `make clean` removes `target/` and `dist/`.

## Usage

```bash
rcca <source> <target> [OPTIONS]
```

The contents of the source directory are copied into the target root;
`--keep-source-name` switches to `dst/<source name>/...`.

The copy is a mirror: files in the target that no longer exist in the source are
deleted (equivalent to `sy <source>/ <target> --delete`). As upstream does by default,
sy refuses when more than 50% of the target files would be deleted; the run then exits
with code 1 and the target stays untouched.

The mirror runs on every invocation, including a source that holds no regular files: an
empty source directory deletes the target's extra files (and is refused by the guard when
that exceeds 50%), a source with only symlinks copies those links, and a tree of empty
directories is created on the target. Such a run verifies nothing and reports
`0/0 passed`.

## Options

- `-j, --jobs <N>`: files handled at once, in both phases; default 1 (serial). The copy
  passes it straight to sy's `-j`; verification runs that many files in parallel, which
  overlaps source reads with target reads. Results are still reported in manifest order.
  With `-j > 1` the progress line shows whichever in-flight file was touched last. The
  value is capped at 64: every worker holds a 1 MiB buffer, so larger values only raise
  memory and thread counts.
- `--keep-source-name`: keep the source directory name in the target layout.
- `--skip-unreadable`: carry on past unreadable source files instead of failing the
  preflight. Each such file is listed once as a problem (with the reason the open failed)
  and the run exits 1. The option cannot make sy skip the file, so the matching per-file
  copy error is folded into that one entry; if sy manages to copy the file anyway, it
  stays unverified because rcca removed it from the manifest.
- `--force`: continue even when the target looks too small.
- `--fail-fast`: abort on the first failure (mapped to sy's `max_errors = 1`). Without it
  there is no limit: every per-file error is collected and the run still prints its
  summary and verifies what was copied.
- `--quiet`: draw nothing, print the summary only.
- `--verbose`: keep and print the complete failure list instead of the first 20.
- `--keep-bad`: quarantine mismatching target files as `.<name>.badhash` instead of
  deleting them.
- `--verify-no-cache`: bypass the page cache while hashing (macOS `F_NOCACHE`, Linux
  `posix_fadvise`).
- `--clean-stale`: remove leftover `.sy-*.tmp` files from the target before starting.
- `--color auto|always|never`: colour policy for the progress line, default `auto`.

## Terminal output

While running there is exactly one line of text, redrawn in place. No progress bars, no
speed, no ETA:

```
SCAN   128 files
COPY     23%    2.6/11.0 GiB  A001_06140008_C008.mov
VERIFY   75%    8.2/11.0 GiB  C7418.MP4
VERIFY  100%   11.0/11.0 GiB  C7418.MP4
```

- Phase labels `SCAN` / `COPY` / `VERIFY` are padded to 6 columns, so the file column
  starts at column 8 and never moves when the phase changes.
- The percentage covers the whole run: the denominator is twice the total source bytes,
  the copy fills the first half and verification the second. During the copy the
  numerator grows by every finished file, skipped files included; during verification it
  grows with the bytes actually read (both sides are read, so that count is halved). A
  fully successful run ends with the numerator equal to the denominator. When the
  denominator is 0 (every source file is empty) the percentage column shows `-`.
- The numerator and the denominator use the denominator's unit and a fixed width, so the
  digits never shift while the line is redrawn.
- Only the file name is shown; over-long names are truncated from the start so the
  extension survives. On a narrow terminal the file name is dropped first, then the byte
  columns, leaving label and percentage.
- The label is dim, everything else uses the default colour. `--color auto|always|never`,
  default `auto` (checks `NO_COLOR`, `TERM=dumb` and whether stderr is a TTY).

The summary on stdout looks like this:

```
SRC  /Volumes/MOD/200 个人目录/David/BlackmagicCamera
DST  /Volumes/MOD/200 个人目录/David/1020 01

  Files      91 files · 5.6 GiB · 02:41
  Copy       0 created · 0 updated · 91 skipped · 0 deleted
  Transfer   0 files · 0 B
  Verify     91 files · 5.6 GiB · 91/91 passed

  ✓ All files copied and verified
```

`Files` is the manifest (elapsed time at the end), `Copy` is sy's own accounting (it
includes directory entries), `Transfer` counts the regular files sy actually moved
(finished transfers only, symlinks excluded), and `Verify` reports the files checked,
their total size and how many passed; a file that was never hashed (target missing,
source changed) contributes nothing to that size. On failure the last line becomes
`! Failed: N problems, listed below` with an indented list of up to 20 examples; the
number is the real total, so `--verbose` is the way to see them all. On Ctrl-C it becomes
`! Interrupted: …` and the exit code is 130.

Symbolic links in the source are copied as links by sy but are not verified (rcca can
only hash regular files); a one-line note on stderr (suppressed by `--quiet`) says so
instead of letting them disappear from the counts.

Special files (fifos, sockets, device nodes) are refused by the preflight with exit
code 3, naming the first one: sy handle only regular files and symlinks, and would read
a fifo as a normal file and block on it forever. Remove the file from the source and run
again.

Progress goes to stderr and stdout carries only the summary, so
`rcca source target > log.txt` yields clean text. When stderr is not a TTY or `--quiet`
is given, nothing is drawn and no thread is started: stderr stays empty.

sy's own planning messages, overall bar and per-file bars are all off; the file name and
byte counts arrive through two callbacks added in `vendor/sy` (see
`vendor/sy/PATCHES.md`). Both phases are serial by default (`-j 1`), so the line always
shows the one file being worked on.

## Exit codes

- 0: copy and verification both succeeded.
- 1: copy failure (including sy errors) or verification failure.
- 2: usage error (source equals or nests with the target, unknown option, ...).
- 3: preflight failure (source unusable, not enough space, 4 GiB cap on FAT, unreadable
  source file, special file in the source, path rule conflict, target not writable).
- 130: interrupted with Ctrl-C during verification.

## Behaviour notes

- sy does the copying: it plans the comparison, transfers, writes to a temp file and
  renames once, so the target never holds a final name with half the content.
- Ctrl-C is not intercepted during the copy: the signal terminates the process and may
  leave `.sy-xxxxxxxx.tmp` files behind, clean them with `--clean-stale`. Only the
  verification phase installs a handler: the first Ctrl-C stops after the file being
  hashed (summary printed, exit code 130), a second one ends the process at once with 130
  and no summary. Verification only checks the flag between files, so the second signal
  is the way out of a long single-file hash.
- Verification compares the XXH64 of source and target from rcca's own manifest, and both
  sides must be regular files: a target that is a symlink, directory or fifo is reported
  as a problem instead of being hashed through (a link pointing at the source would
  otherwise hash equal and pass). Mismatching target files are deleted by default,
  `--keep-bad` quarantines them, so a final file name always means "verified".
- Symbolic links are copied as links by sy but are not verified (only regular files are).
- Metadata: sy keeps the modification time and the permission bits.
- The free-space preflight assumes a full copy (102% of the source bytes plus 64 MiB), so
  a re-run into a nearly full target can be refused even though most files will be
  skipped; `--force` overrides it. On platforms where rcca cannot read the volume (the
  current Windows build), the free-space and 4 GiB FAT checks are skipped and a warning
  is printed instead of failing silently.
- Without a TTY or with `--quiet` nothing is drawn, only the summary is printed.

## Tests

```bash
cargo test                                        # 95 tests (61 unit + 34 integration)
cargo clippy --all-targets -- -D warnings
```

Integration tests cover: full copy and verify, overwriting, mirror delete,
`--keep-source-name` layout, quiet mode, second run skipping unchanged files, mtime and
mode preservation, symlinks (including a symlink-only source and a symlinked target that
must not pass verification), an empty source against a non-empty and an empty target,
empty directories in the source, `--clean-stale`, idempotency, nested target (exit code 2,
no directory left behind, also with a `..` in the target), unreadable source (exit code 3,
`--skip-unreadable` reported once, also for a nested path), a fifo and a socket in the
source (exit code 3), `--fail-fast` aborting on the first error, the delete guard refusing
(exit code 1), and no partial final file after an interrupt.

## Measured (macOS arm64, APFS, 120 files of 3 MiB)

- First run: 120 files copied and verified; the second run transfers nothing (sy reports
  them as skipped) while verification still hashes everything.
- Symlinks are copied as links; source mtime and permission bits match on the target.
- The Windows artifact is cross compiled on `x86_64-pc-windows-gnu` and only depends on
  system DLLs.

## License

MIT, see [LICENSE](LICENSE). Copyright (c) 2026 winddyx.

The copy engine under `vendor/sy` is a modified copy of
[nijaru/sy](https://github.com/nijaru/sy) v0.4.1, MIT licensed, Copyright (c) 2025
Nick Russo; its license text is kept at `vendor/sy/LICENSE` and the local changes are
listed in `vendor/sy/PATCHES.md`.
