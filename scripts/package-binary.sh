#!/bin/sh
# Package a built rondi executable as rondi-VERSION-TRIPLE.tar.gz plus a
# .sha256 file in OUTDIR. The archive unpacks to one directory holding the
# executable, the licenses and the compat-link installer.
set -eu

if [ "$#" -ne 3 ]; then
    printf 'usage: %s PATH_TO_RONDI_EXECUTABLE TARGET_TRIPLE OUTDIR\n' "$0" >&2
    exit 64
fi

binary=$1
triple=$2
outdir=$3
root=$(cd "$(dirname "$0")/.." && pwd)

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n 1)
if [ -z "$version" ]; then
    printf 'no workspace.package.version in %s/Cargo.toml\n' "$root" >&2
    exit 65
fi

stem="rondi-$version-$triple"
staging=$(mktemp -d)
trap 'rm -rf "$staging"' EXIT
dir="$staging/$stem"

mkdir -p "$dir/bin" "$dir/scripts" "$outdir"
cp "$binary" "$dir/bin/rondi"
chmod 0755 "$dir/bin/rondi"
cp "$root/scripts/install-compat-links.sh" "$dir/scripts/"
for file in README.md CHANGELOG.md LICENSE-MIT LICENSE-APACHE LICENSE-GPL-2.0 COPYRIGHT THIRD-PARTY-NOTICES.md; do
    cp "$root/$file" "$dir/"
done

outdir=$(cd "$outdir" && pwd)
# COPYFILE_DISABLE keeps macOS tar from adding AppleDouble entries.
(cd "$staging" && COPYFILE_DISABLE=1 tar -cf - "$stem" | gzip -9n > "$outdir/$stem.tar.gz")
(cd "$outdir" && if command -v sha256sum > /dev/null; then
    sha256sum "$stem.tar.gz"
else
    shasum -a 256 "$stem.tar.gz"
fi > "$stem.tar.gz.sha256")
printf '%s\n' "$outdir/$stem.tar.gz"
