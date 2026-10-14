#!/usr/bin/env bash
# Regenerate the .rrd seed corpus with a local rrdtool (64-bit layout).
set -euo pipefail
out=${1:?output dir}
mkdir -p "$out"; cd "$out"
T=1000000000
upd() { local f=$1 n=$2 v=$3 i; local args=(); for ((i=1;i<=n;i++)); do args+=("$((T+i*10)):$v"); done; rrdtool update "$f" "${args[@]}"; }
rrdtool create gauge_v3.rrd --start $T --step 10 DS:g:GAUGE:30:0:100 RRA:AVERAGE:0.5:1:5 RRA:MIN:0.5:2:3 RRA:MAX:0.5:2:3 RRA:LAST:0.5:1:4
rrdtool create gauge_empty.rrd --start $T --step 10 DS:g:GAUGE:30:U:U RRA:AVERAGE:0.5:1:3
cp gauge_v3.rrd gauge_v3_upd.rrd; upd gauge_v3_upd.rrd 7 42
rrdtool create counter.rrd --start $T --step 10 DS:c:COUNTER:30:U:U DS:d:DERIVE:30:-10:10 DS:a:ABSOLUTE:30:0:U RRA:AVERAGE:0.5:1:4 RRA:MAX:0:3:2
upd counter.rrd 6 "100:5:7"
rrdtool create dcounter_v5.rrd --start $T --step 10 DS:x:DCOUNTER:30:U:U DS:y:DDERIVE:30:U:U RRA:AVERAGE:0.5:1:4 RRA:LAST:0.5:1:3
upd dcounter_v5.rrd 5 "1.5:2.5"
rrdtool create compute.rrd --start $T --step 10 DS:a:GAUGE:30:U:U 'DS:b:COMPUTE:a,2,*' RRA:AVERAGE:0.5:1:4
upd compute.rrd 4 3
rrdtool create hw.rrd --start $T --step 10 DS:g:GAUGE:30:U:U RRA:AVERAGE:0.5:1:4 RRA:HWPREDICT:6:0.1:0.0035:3
upd hw.rrd 8 5
rrdtool create mhw.rrd --start $T --step 10 DS:g:GAUGE:30:U:U RRA:AVERAGE:0.5:1:4 RRA:MHWPREDICT:6:0.1:0.0035:3:1 RRA:SEASONAL:3:0.1:1 RRA:DEVSEASONAL:3:0.1:1 RRA:DEVPREDICT:6:3 RRA:FAILURES:6:2:3:3
upd mhw.rrd 8 7
rrdtool create wrapped.rrd --start $T --step 10 DS:g:GAUGE:30:U:U RRA:AVERAGE:0.5:1:3 RRA:AVERAGE:0.5:4:2
upd wrapped.rrd 13 9
