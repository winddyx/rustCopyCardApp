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

`make dist` produces:

- `dist/rcca-macos-arm64`: macOS arm64 executable.
- `dist/rcca-windows-x64.exe`: Windows x64 executable (PE32+, system DLLs only).
- `dist/SHA256SUMS`: SHA-256 of both, verify with `cd dist && shasum -a 256 -c SHA256SUMS`.

Cross compiling for Windows needs `rustup target add x86_64-pc-windows-gnu` and
`brew install mingw-w64`. `make dist` calls `cargo build --release --target` after
deleting `dist/`, so no stale artifact survives.

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

## Options

- `-j, --jobs <N>`: parallel file transfers handed to sy (its `-j`), default 1
  (serial). Raise it only if you do not need a stable "current file".
- `--keep-source-name`: keep the source directory name in the target layout.
- `--skip-unreadable`: skip unreadable source files instead of failing the preflight.
- `--force`: continue even when the target looks too small.
- `--fail-fast`: abort on the first failure (mapped to sy's `max_errors = 1`).
- `--quiet`: draw nothing, print the summary only.
- `--verbose`: print the complete failure list.
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
  fully successful run ends with the numerator equal to the denominator.
- The numerator and the denominator use the denominator's unit and a fixed width, so the
  digits never shift while the line is redrawn.
- Only the file name is shown; over-long names are truncated from the start so the
  extension survives. On a narrow terminal the file name is dropped first, then the byte
  columns, leaving label and percentage.
- The label is dim, everything else uses the default colour. `--color auto|always|never`,
  default `auto` (checks `NO_COLOR`, `TERM=dumb` and whether stderr is a TTY).

Progress goes to stderr and stdout carries only the summary, so
`rcca source target > log.txt` yields clean text. When stderr is not a TTY or `--quiet`
is given, nothing is drawn and no thread is started: stderr stays empty.

sy's own planning messages, overall bar and per-file bars are all off; the file name and
byte counts arrive through two callbacks added in `vendor/sy` (see
`vendor/sy/PATCHES.md`). The copy is serial by default (`-j 1`), so the line always shows
the one file being worked on.

## Exit codes

- 0: copy and verification both succeeded.
- 1: copy failure (including sy errors) or verification failure.
- 2: usage error (source equals or nests with the target, unknown option, ...).
- 3: preflight failure (source unusable, not enough space, 4 GiB cap on FAT, unreadable
  source file, path rule conflict, target not writable).
- 130: interrupted with Ctrl-C during verification.

## Behaviour notes

- sy does the copying: it plans the comparison, transfers, writes to a temp file and
  renames once, so the target never holds a final name with half the content.
- Ctrl-C is not intercepted during the copy: the signal terminates the process and may
  leave `.sy-xxxxxxxx.tmp` files behind, clean them with `--clean-stale`. Only the
  verification phase installs a handler and exits with 130.
- Verification compares the XXH64 of source and target from rcca's own manifest;
  mismatching target files are deleted by default, `--keep-bad` quarantines them, so a
  final file name always means "verified".
- Symbolic links are copied as links by sy but are not verified (only regular files are).
- Metadata: sy keeps the modification time and the permission bits.
- Without a TTY or with `--quiet` nothing is drawn, only the summary is printed.

## Tests

```bash
cargo test                                        # 62 tests (42 unit + 20 integration)
cargo clippy --all-targets -- -D warnings
```

Integration tests cover: full copy and verify, overwriting, mirror delete,
`--keep-source-name` layout, quiet mode, second run skipping unchanged files, mtime and
mode preservation, symlinks, `--clean-stale`, idempotency, nested target (exit code 2),
unreadable source (exit code 3 and `--skip-unreadable`), the delete guard refusing (exit
code 1), and no partial final file after an interrupt.

## Measured (macOS arm64, APFS, 120 files of 3 MiB)

- First run: 120 files copied and verified; the second run transfers nothing (sy reports
  them as skipped) while verification still hashes everything.
- Symlinks are copied as links; source mtime and permission bits match on the target.
- The Windows artifact is cross compiled on `x86_64-pc-windows-gnu` and only depends on
  system DLLs.
