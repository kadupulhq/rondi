#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
kadupul_root="$(cd "${KADUPUL_ROOT:-$repo_root/../kadupul}" && pwd)"
rrdtool_bin="$(command -v "${RRDTOOL_BIN:-rrdtool}")"
rondi_bin="${RONDI_BIN:-$repo_root/target/debug/rondi}"
php_runner=(mise exec -- php)

if [[ ! -x "$rondi_bin" ]]; then
	printf 'Rondi executable is missing or not executable: %s\n' "$rondi_bin" >&2
	exit 2
fi
if [[ ! -f "$kadupul_root/lib/rrd.php" ]]; then
	printf 'Kadupul checkout is missing lib/rrd.php: %s\n' "$kadupul_root" >&2
	exit 2
fi
if ! "$rrdtool_bin" --version | grep -q '1\.11\.0'; then
	printf 'The differential oracle must be RRDtool 1.11.0: %s\n' "$rrdtool_bin" >&2
	exit 2
fi

tmp="$(mktemp -d "${TMPDIR:-/tmp}/rondi-poller-smoke.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT

"$rrdtool_bin" create "$tmp/oracle.rrd" --start 1000000000 --step 10 \
	DS:a:GAUGE:60:U:U DS:b:GAUGE:60:U:U RRA:AVERAGE:0.5:1:8
cp "$tmp/oracle.rrd" "$tmp/rondi.rrd"
ln -s "$rondi_bin" "$tmp/rrdtool"

cat > "$tmp/poller-smoke.php" <<'PHP'
<?php
define('CACTI_LOCALE', 'en-US');
function read_config_option($name) {
    return $name === 'path_rrdtool' ? getenv('RRDTOOL_EXE') : '';
}
$config = ['cacti_server_os' => 'unix'];
require getenv('KADUPUL_ROOT') . '/lib/rrd.php';
$pipe = rrd_init();
if (!is_resource($pipe)) {
    exit(10);
}
$file = $argv[1];
foreach ([
    "update $file --skip-past-updates --template b:a 1000000010:20:10",
    "update $file --skip-past-updates --template b:a 1000000020:40:30",
] as $command) {
    if (fwrite($pipe, escape_command($command) . "\r\n") === false) {
        exit(11);
    }
    fflush($pipe);
}
rrd_close($pipe);
PHP

KADUPUL_ROOT="$kadupul_root" RRDTOOL_EXE="$rrdtool_bin" "${php_runner[@]}" "$tmp/poller-smoke.php" "$tmp/oracle.rrd"
KADUPUL_ROOT="$kadupul_root" RRDTOOL_EXE="$tmp/rrdtool" "${php_runner[@]}" "$tmp/poller-smoke.php" "$tmp/rondi.rrd"
cmp "$tmp/oracle.rrd" "$tmp/rondi.rrd"
printf 'Kadupul rrd_init/template update stream matches RRDtool 1.11.0 bytes.\n'
