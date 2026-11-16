#!/usr/bin/env bash
#
# Builds extensions into the shape the host loads them in.
#
#   scripts/build-extension.sh                 # every extension in extensions/
#   scripts/build-extension.sh inbox-stats     # just that one
#   scripts/build-extension.sh path/to/ext     # or one anywhere else
#
# An extension is a WebAssembly component plus its manifest, in a directory
# named after its id:
#
#   target/extensions/inbox-stats/spool.toml
#   target/extensions/inbox-stats/extension.wasm
#
# Copy one of those directories into ~/.local/share/spool/extensions to install
# it. That is the whole install process — there is no registry yet, and a
# directory is a thing you can inspect before you trust it.
set -euo pipefail

TARGET=wasm32-wasip2
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/target/extensions"

# Finding a toolchain that can actually build for wasm.
#
# `rustc --print target-libdir` answers for a target whether or not its standard
# library is installed — it prints where the files *would* be — so the probe has
# to look for the directory. This matters on a machine with more than one Rust:
# a Homebrew rustc earlier in PATH than rustup's will happily claim the target
# and then fail to find `core`.
has_target() {
    local dir
    dir="$("$1" --print target-libdir --target "$TARGET" 2>/dev/null)" || return 1
    [ -d "$dir" ]
}

CARGO=""
RUSTC=""

if has_target "$(command -v rustc 2>/dev/null || echo /nonexistent)"; then
    CARGO="$(command -v cargo)"
    RUSTC="$(command -v rustc)"
elif command -v rustup >/dev/null 2>&1 && has_target "$(rustup which rustc 2>/dev/null || echo /nonexistent)"; then
    CARGO="$(rustup which cargo)"
    RUSTC="$(rustup which rustc)"
fi

if [ -z "$RUSTC" ]; then
    echo "No Rust toolchain here can build for $TARGET." >&2
    echo "Install it with:  rustup target add $TARGET" >&2
    exit 1
fi

# `RUSTC` is passed explicitly rather than left to PATH, because cargo resolves
# rustc through PATH and the one it would find is not necessarily the one that
# has the target.
build_one() {
    local dir="$1"
    local manifest="$dir/spool.toml"

    if [ ! -f "$manifest" ]; then
        echo "skipping $(basename "$dir"): no spool.toml" >&2
        return 0
    fi

    # The id, from the manifest, so the output directory and the manifest can
    # never disagree — which the host checks for and refuses.
    local id
    id="$(sed -n 's/^[[:space:]]*id[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$manifest" | head -1)"
    if [ -z "$id" ]; then
        echo "skipping $(basename "$dir"): its spool.toml declares no id" >&2
        return 1
    fi

    echo "building $id"
    RUSTC="$RUSTC" "$CARGO" build --release --target "$TARGET" \
        --manifest-path "$dir/Cargo.toml"

    # cargo spells a crate's artifact with underscores whatever the directory
    # is called.
    local crate
    crate="$(sed -n 's/^name[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$dir/Cargo.toml" | head -1)"
    local wasm="$dir/target/$TARGET/release/${crate//-/_}.wasm"

    if [ ! -f "$wasm" ]; then
        echo "$id built but produced no $wasm" >&2
        return 1
    fi

    mkdir -p "$OUT/$id"
    cp "$wasm" "$OUT/$id/extension.wasm"
    cp "$manifest" "$OUT/$id/spool.toml"

    echo "  -> $OUT/$id ($(wc -c < "$OUT/$id/extension.wasm" | tr -d ' ') bytes)"
}

if [ $# -gt 0 ]; then
    for name in "$@"; do
        # A path if it is one — test fixtures live with the tests rather than
        # in `extensions/`, which is for extensions somebody might want.
        if [ -d "$name" ]; then
            build_one "$(cd "$name" && pwd)"
        else
            build_one "$ROOT/extensions/$name"
        fi
    done
else
    for dir in "$ROOT"/extensions/*/; do
        # The SDK is the crate extensions are written against, not an
        # extension. It has no manifest, so it is skipped by that test alone.
        build_one "${dir%/}"
    done
fi
