#!/bin/sh
# Runs after the package's files are installed, as Debian postinst
# ("configure" [old-version]) and as RPM %post (1 on install, 2 or more on
# upgrade).
set -e

# Create the rondi service account. Without systemd-sysusers (minimal
# containers), fall back to useradd with the same settings as sysusers.d.
if command -v systemd-sysusers > /dev/null 2>&1; then
    systemd-sysusers rondi.conf
elif ! getent passwd rondi > /dev/null 2>&1; then
    useradd --system --user-group --home-dir /var/lib/rondi --no-create-home \
        --shell /usr/sbin/nologin --comment "Rondi rrdcached" rondi
fi

if [ -d /run/systemd/system ]; then
    systemctl daemon-reload || :
    upgrade=false
    case "$1" in
        configure) [ -n "${2:-}" ] && upgrade=true ;;
        [2-9] | [1-9][0-9]*) upgrade=true ;;
    esac
    # The unit is never enabled here; an upgrade only restarts it if running.
    if [ "$upgrade" = true ]; then
        systemctl try-restart rondi-rrdcached.service || :
    fi
fi
