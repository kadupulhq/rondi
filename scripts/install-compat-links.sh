#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
    printf 'usage: %s PATH_TO_RONDI_EXECUTABLE BIN_DIRECTORY\n' "$0" >&2
    exit 64
fi

binary=$1
bindir=$2
case "$binary" in
    /*) ;;
    *) binary="$(pwd)/$binary" ;;
esac
if [ ! -x "$binary" ]; then
    printf 'not an executable: %s\n' "$binary" >&2
    exit 66
fi
mkdir -p "$bindir"
for name in rrdtool rrdcached rrdupdate rrdcreate rrdinfo rrdtool-proxy rrdproxy; do
    link="$bindir/$name"
    if [ -e "$link" ] || [ -L "$link" ]; then
        if [ ! -L "$link" ] || [ "$(readlink "$link")" != "$binary" ]; then
            printf 'refusing to replace existing path: %s\n' "$link" >&2
            exit 73
        fi
    fi
done
for name in rrdtool rrdcached rrdupdate rrdcreate rrdinfo rrdtool-proxy rrdproxy; do
    link="$bindir/$name"
    if [ ! -L "$link" ]; then
        ln -s "$binary" "$link"
    fi
done
