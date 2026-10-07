<?php
/*
 * Invocation adapter for deployments whose service unit runs:
 *     php rrdtool-proxy.php [arguments]
 * The RRDProxy implementation remains in the shared Rust executable.
 */

$binary = getenv('RONDI_BIN');
if ($binary === false || $binary === '') {
    $binary = __DIR__ . DIRECTORY_SEPARATOR . 'rrdtool-proxy';
}

if (!is_file($binary) || !is_executable($binary)) {
    fwrite(STDERR, "rrdtool-proxy.php: Rondi executable not found: {$binary}\n");
    exit(127);
}

$command = array_merge(array($binary, '--rrdproxy-launcher'), array_slice($argv, 1));
$process = proc_open($command, array(
    0 => STDIN,
    1 => STDOUT,
    2 => STDERR,
), $pipes);

if (!is_resource($process)) {
    fwrite(STDERR, "rrdtool-proxy.php: failed to start Rondi executable\n");
    exit(126);
}

exit(proc_close($process));
