#!/bin/sh
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
sys="$(uname -s)"
case "$sys" in
MINGW* | MSYS* | CYGWIN*) rel="x86_64-pc-windows-msvc/tasks.exe" ;;
Darwin*)
    case "$(uname -m)" in
    arm64) rel="aarch64-apple-darwin/tasks" ;;
    *) rel="x86_64-apple-darwin/tasks" ;;
    esac ;;
*)
    case "$(uname -m)" in
    aarch64 | arm64) rel="aarch64-unknown-linux-musl/tasks" ;;
    *) rel="x86_64-unknown-linux-musl/tasks" ;;
    esac ;;
esac
if [ -x "$here/$rel" ]; then exec "$here/$rel" "$@"; fi
for cand in "$here/target/release/tasks.exe" "$here/target/release/tasks" "$here/target/debug/tasks.exe" "$here/target/debug/tasks"; do
    if [ -x "$cand" ]; then exec "$cand" "$@"; fi
done
echo "tasks binary not found in $here" >&2
exit 1
