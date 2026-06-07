#!/bin/sh
# Oracle entrypoint: render a WaveJSON fixture with the REAL wavedrom.js and
# emit the resulting SVG on stdout.
#
# The fixture arrives on STDIN (NO bind mount) so the container never touches
# the host filesystem — this sidesteps SELinux mount-label denials, exactly
# like the dagre-compare oracle. We stash stdin in /tmp (writable, in-image)
# because wavedrom-cli takes file paths, not stdin.
#
#   docker run --rm -i wavedrom-compare-oracle < fixture.json
#
# wavedrom-cli flags (v3.2.0):  -i <input>  -s <output.svg>
set -eu

cat > /tmp/in.json5

# wavedrom-cli writes the SVG to the -s path; it prints progress noise on
# stdout/stderr, so we route it to stderr and only the SVG file goes to stdout.
wavedrom-cli -i /tmp/in.json5 -s /tmp/out.svg 1>&2

cat /tmp/out.svg
