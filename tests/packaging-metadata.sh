#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
control="$root/debian/control"
manifest="$root/debian/regolith-displayd.manpages"

grep -Fx 'Section: admin' "$control" >/dev/null
grep -Fx 'Homepage: https://github.com/regolith-linux/regolith-displayd' "$control" >/dev/null
grep -Fx 'Description: Daemon for enabling inter-op between' "$control" >/dev/null

awk '
    /^Description: / { in_description = 1; next }
    in_description && /^[^ ]/ { in_description = 0 }
    in_description {
        if ($0 !~ /^ [^[:space:]]/ || $0 ~ /[[:blank:]]$/) {
            exit 1
        }
    }
' "$control"

test -f "$manifest"
for page in regolith-displayd.1 regolith-displayd-init.1; do
    grep -Fx "debian/man/$page" "$manifest" >/dev/null
    test -f "$root/debian/man/$page"
    grep -Eq '^\.TH [A-Z0-9-]+ 1 ' "$root/debian/man/$page"
    grep -Fx '.SH NAME' "$root/debian/man/$page" >/dev/null
    grep -Fx '.SH DESCRIPTION' "$root/debian/man/$page" >/dev/null
done

printf '%s\n' 'displayd packaging metadata regression: PASS'
