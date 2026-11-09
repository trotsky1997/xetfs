#!/bin/sh
set -eu
if [ "$#" -ne 1 ]; then
    printf 'Usage: %s NEW_OUTPUT_DIRECTORY\n' "$0" >&2
    exit 2
fi
extension_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
macos_dir=$(dirname "$extension_dir")
output=$1
if ! mkdir "$output"; then
    printf 'Output directory must be new: %s\n' "$output" >&2
    exit 1
fi
output=$(CDPATH= cd -- "$output" && pwd)
target_dir=${CARGO_TARGET_DIR:-"$macos_dir/target"}
cargo build --locked --manifest-path "$macos_dir/Cargo.toml" --target-dir "$target_dir"
app="$output/XetFS.app/Contents"
ext="$app/PlugIns/XetFS.appex/Contents"
mkdir -p "$app/MacOS" "$ext/MacOS"
cp "$extension_dir/HostInfo.plist" "$app/Info.plist"
cp "$extension_dir/Info.plist" "$ext/Info.plist"
swiftc "$extension_dir/host.swift" -framework AppKit -o "$app/MacOS/XetFS"
swiftc -parse-as-library "$extension_dir/main.swift" "$target_dir/debug/libxetfs_fskit.a" \
    -framework FSKit -framework Foundation -framework OpenDirectory \
    -framework SystemConfiguration -framework IOKit -framework Security \
    -framework CoreFoundation -o "$ext/MacOS/XetFSExtension"
printf 'Unsigned app built: %s\n' "$app"
