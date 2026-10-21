#!/usr/bin/env bash
# Cross C compiler for the Linux musl targets: zig ships a full musl sysroot.
# cc-rs appends its own --target=<rust triple>, which zig does not understand, so it
# is dropped here and the zig spelling (RCCA_ZIG_TARGET) is used instead.
set -euo pipefail
target="${RCCA_ZIG_TARGET:?RCCA_ZIG_TARGET must be set}"
args=()
for arg in "$@"; do
    case "$arg" in
        --target=*) ;;
        *) args+=("$arg") ;;
    esac
done
exec zig cc -target "$target" "${args[@]}"
