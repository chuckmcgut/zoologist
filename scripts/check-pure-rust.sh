#!/usr/bin/env bash
# Fails if the dependency tree links a C/C++ library (*-sys crates) or compiles native code
# with the `cc` crate, except the allowlisted cases explained in IMPLEMENTATION_PLAN.md §0 rule 3.
set -euo pipefail
cd "$(dirname "$0")/.."

# Crates that are pure Rust despite the -sys suffix or that only declare OS APIs.
ALLOW='^(linux-raw-sys|windows-sys|windows-targets|libc)$'
# Crates allowed to use `cc` at build time: tract assembles its own SIMD kernels.
CC_ALLOW='^(tract-linalg)$'

# Judge the deployment target (the Docker image), not the developer's machine.
TARGET="${TARGET:-x86_64-unknown-linux-gnu}"
tree() { cargo tree --quiet --workspace --target "$TARGET" "$@" 2>/dev/null; }

bad=$(tree -e normal,build --prefix none --format '{p}' | awk '{print $1}' | sort -u | grep -E -- '-sys$' | grep -Ev "$ALLOW" || true)
if [ -n "$bad" ]; then
  echo "Non-Rust (-sys) crates found:"
  echo "$bad"
  exit 1
fi

if tree -e normal,build -i cc >/dev/null 2>&1; then
  users=$(tree -e normal,build -i cc --depth 1 --prefix none --format '{p}' \
    | awk '{print $1}' | grep -v '^cc$' | sort -u | grep -Ev "$CC_ALLOW" || true)
  if [ -n "$users" ]; then
    echo "Crates compiling native code via 'cc':"
    echo "$users"
    exit 1
  fi
fi

echo "pure-rust check OK"
