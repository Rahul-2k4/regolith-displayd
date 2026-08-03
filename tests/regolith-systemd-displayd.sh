#!/bin/bash
set -Eeu -o pipefail

ROOT_DIR=$(cd "$(dirname "$0")/.." && pwd)
DISPLAYD_SERVICE="$ROOT_DIR/data/regolith-init-displayd.service"
KANSHI_SERVICE="$ROOT_DIR/data/regolith-init-kanshi.service"

fail() { echo "displayd systemd metadata test: $*" >&2; exit 1; }
has_line() { grep -Fqx "$2" "$1"; }
section_has_key() {
    local file="$1" section="$2" key="$3"
    awk -v wanted="[$section]" -v key="$key" '
        /^\[/ { current = $0 }
        current == wanted && $0 ~ "^" key "=" { found = 1 }
        END { exit found ? 0 : 1 }
    ' "$file"
}
section_has_line() {
    local file="$1" section="$2" expected="$3"
    awk -v wanted="[$section]" -v expected="$expected" '
        /^\[/ { current = $0 }
        current == wanted && $0 == expected { found = 1 }
        END { exit found ? 0 : 1 }
    ' "$file"
}
check_common_metadata() {
    local service="$1"
    [ -f "$service" ] || fail "missing service: $service"
    if grep -Fq "Wants=gnome-session.target" "$service"; then fail "old GNOME Wants remains in $service"; fi
    if grep -Fq "After=gnome-session.target" "$service"; then fail "old GNOME After remains in $service"; fi
    if grep -Fq "WantedBy=regolith-wayland.target" "$service"; then fail "old regolith-wayland WantedBy remains in $service"; fi
    section_has_key "$service" Unit StartLimitIntervalSec || fail "StartLimitIntervalSec is not in [Unit] in $service"
    if section_has_key "$service" Service StartLimitIntervalSec; then fail "StartLimitIntervalSec remains in [Service] in $service"; fi
}
check_common_metadata "$DISPLAYD_SERVICE"
has_line "$DISPLAYD_SERVICE" "PartOf=graphical-session.target" || fail "displayd graphical-session ownership is missing"
if grep -Eq "^PartOf=.*regolith-(gnome|cosmic)\\.target" "$DISPLAYD_SERVICE"; then fail "displayd has mutually exclusive Regolith target ownership"; fi
has_line "$DISPLAYD_SERVICE" "WantedBy=regolith-gnome.target regolith-cosmic.target" || fail "displayd target install wiring is missing"
if grep -Fq "Requires=regolith-init-kanshi.service" "$DISPLAYD_SERVICE"; then fail "displayd still requires kanshi"; fi
if grep -Fq "Before=regolith-init-kanshi.service" "$DISPLAYD_SERVICE"; then fail "displayd still orders before kanshi"; fi
check_common_metadata "$KANSHI_SERVICE"
has_line "$KANSHI_SERVICE" "PartOf=regolith-gnome.target" || fail "kanshi target ownership is missing"
has_line "$KANSHI_SERVICE" "WantedBy=regolith-gnome.target" || fail "kanshi target install wiring is missing"
section_has_line "$KANSHI_SERVICE" Unit "After=cosmic-session.target" || fail "kanshi must start after cosmic session target"
section_has_line "$KANSHI_SERVICE" Unit "Conflicts=cosmic-session.target" || fail "kanshi must conflict with cosmic session target"
echo "displayd systemd metadata: PASS"
