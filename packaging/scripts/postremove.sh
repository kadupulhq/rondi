#!/bin/sh
# Runs after the package's files are removed, as Debian postrm and RPM
# %postun. The rondi account and /var/lib/rondi are left in place so a
# reinstall keeps file ownership; remove them by hand if wanted.
set -e

if [ -d /run/systemd/system ]; then
    systemctl daemon-reload || :
fi
