#!/bin/sh
set -eu

# 既定値はacceptance workloadの500万logical lineを作る。1行を約220 byteにして、
# 1 GiBと500万行のうち先に到達する境界も同じstreamで観測できるようにする。
lines=${1:-5000000}
payload_width=${2:-200}

case "$lines:$payload_width" in
    *[!0-9:]* | 0:* | *:0)
        echo "usage: $0 [positive-line-count] [positive-payload-width]" >&2
        exit 2
        ;;
esac

LC_ALL=C awk -v lines="$lines" -v width="$payload_width" 'BEGIN {
    payload = ""
    for (column = 0; column < width; column++) {
        payload = payload "x"
    }

    print "diff --git a/large.txt b/large.txt"
    print "--- a/large.txt"
    print "+++ b/large.txt"
    printf "@@ -1,%d +1,%d @@\n", lines, lines
    for (line = 1; line <= lines; line++) {
        printf " line-%07d %s\n", line, payload
    }
}'
