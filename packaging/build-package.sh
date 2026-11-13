#!/bin/sh
# Build a Linux package of a built rondi executable with nfpm.
#
#   packaging/build-package.sh PACKAGER PATH_TO_RONDI_EXECUTABLE ARCH OUTDIR
#
# PACKAGER is an nfpm packager (deb). ARCH is x86_64 or aarch64. Set NFPM
# to the nfpm executable when it is not on PATH, and RONDI_MAINTAINER to
# override the package maintainer field.
set -eu

if [ "$#" -ne 4 ]; then
    printf 'usage: %s PACKAGER PATH_TO_RONDI_EXECUTABLE ARCH OUTDIR\n' "$0" >&2
    exit 64
fi

packager=$1
binary=$2
arch=$3
outdir=$4
root=$(cd "$(dirname "$0")/.." && pwd)
nfpm=${NFPM:-nfpm}

case "$packager" in
    deb) ;;
    *) printf 'unsupported packager: %s\n' "$packager" >&2; exit 64 ;;
esac
case "$arch" in
    x86_64) NFPM_ARCH=amd64 ;;
    aarch64) NFPM_ARCH=arm64 ;;
    *) printf 'unsupported arch: %s\n' "$arch" >&2; exit 64 ;;
esac

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n 1)
if [ -z "$version" ]; then
    printf 'no workspace.package.version in %s/Cargo.toml\n' "$root" >&2
    exit 65
fi

case "$binary" in
    /*) ;;
    *) binary="$(pwd)/$binary" ;;
esac
mkdir -p "$outdir"
outdir=$(cd "$outdir" && pwd)

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# The checked-in unit starts /usr/bin/rrdcached, the path the README's manual
# install uses. The package keeps that name off PATH.
unit="$work/rondi-rrdcached.service"
sed 's#^ExecStart=/usr/bin/rrdcached #ExecStart=/usr/lib/rondi/bin/rrdcached #' \
    "$root/packaging/systemd/rondi-rrdcached.service" > "$unit"
if ! grep -q '^ExecStart=/usr/lib/rondi/bin/rrdcached ' "$unit"; then
    printf 'ExecStart in packaging/systemd/rondi-rrdcached.service did not match\n' >&2
    exit 65
fi

export NFPM_ARCH
export RONDI_VERSION="$version"
export RONDI_RELEASE="${RONDI_RELEASE:-1}"
export RONDI_MAINTAINER="${RONDI_MAINTAINER:-Kadupul <https://github.com/kadupulhq/rondi/issues>}"
export RONDI_BIN="$binary"
export RONDI_UNIT="$unit"

# nfpm resolves relative src paths against its working directory.
cd "$root"
"$nfpm" package --config packaging/nfpm.yaml --packager "$packager" --target "$outdir/"
