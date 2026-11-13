#!/bin/sh
# Install a rondi package as root in a disposable container, exercise the
# installed names, reinstall it, remove it and check nothing is left behind.
#
#   packaging/smoke-test.sh PATH_TO_PACKAGE
set -eu

if [ "$#" -ne 1 ]; then
    printf 'usage: %s PATH_TO_PACKAGE\n' "$0" >&2
    exit 64
fi
package=$1

case "$package" in
    *.deb)
        export DEBIAN_FRONTEND=noninteractive
        apt-get update -qq
        install() { apt-get install --yes -qq "$package"; }
        reinstall() { apt-get install --yes -qq --reinstall "$package"; }
        remove() { apt-get purge --yes -qq rondi; }
        ;;
    *.rpm)
        # Release RPMs are unsigned; their provenance is the GitHub
        # attestation, so the signature check is skipped for the local file.
        if command -v zypper > /dev/null 2>&1; then
            install() { zypper --non-interactive --quiet install --allow-unsigned-rpm "$package"; }
            reinstall() { zypper --non-interactive --quiet install --force --allow-unsigned-rpm "$package"; }
            remove() { zypper --non-interactive --quiet remove rondi; }
        else
            install() { dnf install --assumeyes --quiet --nogpgcheck "$package"; }
            reinstall() { dnf reinstall --assumeyes --quiet --nogpgcheck "$package"; }
            remove() { dnf remove --assumeyes --quiet rondi; }
        fi
        ;;
    *)
        printf 'unsupported package type: %s\n' "$package" >&2
        exit 64
        ;;
esac

install
rondi --version
for name in rrdtool rrdcached rrdupdate rrdcreate rrdinfo rrdtool-proxy rrdproxy; do
    test "$(readlink "/usr/lib/rondi/bin/$name")" = /usr/bin/rondi
done
# The compatibility names must stay off PATH; see packaging/nfpm.yaml.
if command -v rrdtool > /dev/null 2>&1; then
    echo 'rrdtool is on PATH' >&2
    exit 1
fi
getent passwd rondi
grep -q '^ExecStart=/usr/lib/rondi/bin/rrdcached ' \
    /usr/lib/systemd/system/rondi-rrdcached.service

work=$(mktemp -d)
cd "$work"
/usr/lib/rondi/bin/rrdtool create t.rrd --start 1700000000 --step 10 \
    DS:x:GAUGE:20:U:U RRA:AVERAGE:0.5:1:10
/usr/lib/rondi/bin/rrdtool update t.rrd 1700000010:5 1700000020:7
test "$(/usr/lib/rondi/bin/rrdtool lastupdate t.rrd | tail -n 1)" = '1700000020: 7'
/usr/lib/rondi/bin/rrdcached -h | head -n 1 | grep -q '^RRDCacheD 1\.11\.0'
cd /
rm -rf "$work"

reinstall
remove
for path in /usr/bin/rondi /usr/lib/rondi /usr/lib/systemd/system/rondi-rrdcached.service; do
    if [ -e "$path" ]; then
        printf 'left behind after removal: %s\n' "$path" >&2
        exit 1
    fi
done
echo 'smoke test passed'
