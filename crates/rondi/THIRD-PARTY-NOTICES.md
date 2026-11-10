# Third-party notices

Rondi is licensed under MIT OR Apache-2.0 (see `LICENSE-MIT` and
`LICENSE-APACHE`). Two parts of Rondi derive from files of RRDtool 1.11.0 that
carry their own permissive licenses. Those parts are kept with attribution
(maintainer decision D3, recorded in `docs/provenance-audit.md`). The notices
below reproduce the upstream copyright lines and license text as required by
those licenses.

## 1. rrdcached client: RRDtool client library (MIT)

Upstream: `src/rrd_client.c` in RRDtool 1.11.0.

Rondi code derived from it, in `crates/rondi-cli/src/main.rs`:

- `rrdcached_client_path`
- `translate_rrdcached_file_command`
- `connect_rrdcached`
- `split_rrdcached_network_address`

Upstream notice:

```text
RRDtool - src/rrd_client.c
Copyright (C) 2008-2013  Florian octo Forster

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to
deal in the Software without restriction, including without limitation the
rights to use, copy, modify, merge, publish, distribute, sublicense, and/or
sell copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS
IN THE SOFTWARE.

Authors:
  Florian octo Forster <octo at verplant.org>
  Sebastian tokkee Harl <sh at tokkee.org>
```

## 2. Decimal conversion: RRDtool strtod (BSD, 3-clause)

Upstream: `src/rrd_strtod.c` in RRDtool 1.11.0 (originally `strtod.c`, "Convert
string to double").

Rondi code derived from it:

- In `crates/rondi/src/rrd_number.rs`, only the decimal conversion
  `parse_rrd_decimal`, the wrappers `parse_rrd_number` and `rrd_strtodbl`
  that call it, and their special-spelling helper `parse_special`. The rest
  of that file (the `strtol` family) is not derived from it.
- `crates/rondi-cli/src/cparse.rs`

Upstream notice:

```text
rrd_strtod.c

strtod.c

Convert string to double

Copyright (C) 2002 Michael Ringgaard. All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions
are met:

1. Redistributions of source code must retain the above copyright
notice, this list of conditions and the following disclaimer.
2. Redistributions in binary form must reproduce the above copyright
notice, this list of conditions and the following disclaimer in the
documentation and/or other materials provided with the distribution.
3. Neither the name of the project nor the names of its contributors
may be used to endorse or promote products derived from this software
without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT OWNER OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS
OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION)
HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT
LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY
OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF
SUCH DAMAGE.
```
