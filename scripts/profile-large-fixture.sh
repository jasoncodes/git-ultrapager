#!/bin/sh
set -eu

# 既定workloadは500万source line、payload 200 byteで約1 GiBになる。
# Fixture、release build、reportは同じ/tmp directoryへ置き、repositoryやglobal Git設定、installed binaryを変更しない。
if [ "$#" -gt 3 ]; then
    echo "usage: $0 [positive-line-count] [positive-payload-width] [positive-sample-count]" >&2
    exit 2
fi

lines=${1:-5000000}
payload_width=${2:-200}
samples=${3:-101}

case "$samples" in
    *[!0-9]* | 0)
        echo "usage: $0 [positive-line-count] [positive-payload-width] [positive-sample-count]" >&2
        exit 2
        ;;
esac

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_dir=$(CDPATH= cd -- "$script_dir/.." && pwd)
evidence_dir=$(mktemp -d /tmp/git-ultrapager-profile.XXXXXX)
fixture="$evidence_dir/large.diff"
report="$evidence_dir/report.txt"
target_dir="$evidence_dir/target"

echo "generating fixture under $evidence_dir"
"$script_dir/generate-large-fixture.sh" "$lines" "$payload_width" >"$fixture"

status=0
(
    cd "$repo_dir"
    CARGO_TARGET_DIR="$target_dir" cargo run --release --quiet --example large_profile -- \
        "$fixture" "$samples"
) >"$report" 2>&1 || status=$?

cat "$report"
echo "fixture: $fixture"
echo "report: $report"
echo "release artifacts: $target_dir"
exit "$status"
