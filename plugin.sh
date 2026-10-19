#!/bin/sh
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
sys="$(uname -s)"
case "$sys" in
MINGW* | MSYS* | CYGWIN*) rel="x86_64-pc-windows-msvc/gray-credpool.exe" ;;
Darwin*)
    case "$(uname -m)" in
    arm64) rel="aarch64-apple-darwin/gray-credpool" ;;
    *) rel="x86_64-apple-darwin/gray-credpool" ;;
    esac ;;
*)
    case "$(uname -m)" in
    aarch64 | arm64) rel="aarch64-unknown-linux-musl/gray-credpool" ;;
    *) rel="x86_64-unknown-linux-musl/gray-credpool" ;;
    esac ;;
esac
if [ -x "$here/$rel" ]; then exec "$here/$rel" "$@"; fi
for cand in "$here/target/release/gray-credpool.exe" "$here/target/release/gray-credpool" "$here/target/debug/gray-credpool.exe" "$here/target/debug/gray-credpool"; do
    if [ -x "$cand" ]; then exec "$cand" "$@"; fi
done
echo "gray-credpool binary not found in $here" >&2
exit 1
