# rustCopyCardApp project conventions

## Identity

- Package `rustcopycardapp`, library and binary both named `rcca` (see `[lib]` and `[[bin]]`
  in `Cargo.toml`).
- Version numbers travel with git tags: a behaviour change bumps the version and adds
  `git tag -a vX.Y.Z`.
- Working branch `sy-wrapper` (the copy goes through the embedded sy). `main` still holds
  the hand-written engine of `v1.0.2`; it is reference only and no longer evolves.
- No remote is configured; per the global rules only a personal fork is ever pushed,
  never upstream.

## Architecture boundaries

- Do not reimplement the copy: `src/engine.rs` drives the embedded sy (upstream
  `nijaru/sy` v0.4.1, path dependency on `vendor/sy`).
- The entry point must be
  `SyncEngine::with_config(LocalTransport::new(), config).sync(source, target)`. The
  other route, `SyncSession` (which goes through `TaskExecutor`), prints no progress for
  local copies and leaves the terminal blank.
- The source path is passed by content (`<source>/`): the target receives the contents of
  the source directory, not one more level named after it.
- Mirror semantics are fixed to `--delete` plus `max_delete 50%` (the upstream guard);
  no further switches are exposed.
- rcca owns: preflight (path nesting, writable target, free space, the 4 GiB cap on FAT,
  unreadable source files), the manifest, XXH64 verification, the single-line interface,
  the summary and the exit codes.

## vendor/sy

- Keep only `src/`, `Cargo.toml` and `LICENSE`; patches and upgrade steps live in
  `vendor/sy/PATCHES.md`.
- Three Windows patches (`Win32_Foundation` in `windows-sys`, `dest_path` in `ssh.rs`,
  `symlink_file`/`symlink_dir`) are what makes `x86_64-pc-windows-gnu` build at all. One
  further patch adds callbacks (file started, bytes per finished task) so rcca can turn
  off every sy progress output and draw its own line. Any vendor change must update
  `PATCHES.md`.
- Never file upstream PRs or issues; behaviour differences are made inside `vendor/sy`
  and documented there.

## Commands

- `make dist`: wipe `dist/`, build `aarch64-apple-darwin` and `x86_64-pc-windows-gnu`,
  produce `dist/rcca-macos-arm64`, `dist/rcca-windows-x64.exe` and `dist/SHA256SUMS`.
- `make test` (that is `cargo test`): 42 unit tests plus 20 integration tests.
- `make lint`: `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check`.
- Cross compiling for Windows needs `rustup target add x86_64-pc-windows-gnu` and
  Homebrew's `mingw-w64`.

## Behaviour to keep

- Exit codes: 0 all good; 1 copy failure (including sy errors) or verification failure;
  2 usage error; 3 preflight failure; 130 Ctrl-C during verification.
- Ctrl-C is not intercepted during the copy (the default signal handling terminates the
  process and may leave `.sy-xxxxxxxx.tmp`, cleaned by `--clean-stale`); only the
  verification phase installs a handler and exits with 130.
- When more than 50% of the target would be deleted sy refuses, the run exits with 1 and
  the target stays untouched. That upstream guard is kept on purpose.

## Terminal output (easiest place to break things)

- No progress bars, no speed, no ETA. Exactly one line, redrawn in place:
  `label  percentage  done/total  current file`.
- The percentage denominator is twice the total source bytes: the copy fills the first
  half, verification the second. During the copy the numerator includes skipped files;
  during verification it is the bytes read, halved.
- Labels `SCAN` / `COPY` / `VERIFY` are padded to 6 columns, the file name starts at
  column 8; over-long names are truncated from the start so the extension survives.
- Progress goes to stderr, stdout carries only the summary. Without a TTY on stderr or
  with `--quiet` nothing is drawn, no thread starts and stderr stays empty.
- Every sy progress output stays off (`quiet=true`, `progress=false`); the file name and
  byte counts come from the two callbacks in `vendor/sy`.
- The copy is serial by default (`-j 1`, `max_concurrent`). Do not raise it without a
  reason: with parallel transfers the "current file" jumps between files.
- The label is dim, everything else default colour; `--color auto|always|never`, and
  `NO_COLOR` or `TERM=dumb` disables colour.

## Definition of done

- After touching the interface, verification or stats: `make test` and `make lint` green,
  then `make dist` to confirm both artifacts and `SHA256SUMS`.
- Interface changes update the `src/ui.rs` unit tests (column alignment, degradation
  order, width bound) and the sample in `README.md`.
- Changes that touch sy semantics (mirror delete, skip, threshold) run
  `tests/cli_flow.rs` and `tests/failure_paths.rs` and keep the exit codes above intact.

## Documentation

- `README.md` is for users: build, options, exit codes, interface sample. No separate
  design documents; the code is the specification.
- Behaviour changes update the README together with the code, not only the commit message.
