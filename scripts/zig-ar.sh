#!/usr/bin/env bash
# Archiver for the Linux musl targets; zig provides a compatible `ar`.
set -euo pipefail
exec zig ar "$@"
