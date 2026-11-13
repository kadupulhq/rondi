#!/bin/sh
# Runs before the package's files are removed, as Debian prerm ("remove" on
# removal, "upgrade" on upgrade) and as RPM %preun (0 on erase, 1 on upgrade).
# Only a removal stops and disables the service.
set -e

case "$1" in
    remove | 0) ;;
    *) exit 0 ;;
esac

if [ -d /run/systemd/system ]; then
    systemctl --no-reload disable --now rondi-rrdcached.service > /dev/null 2>&1 || :
fi
