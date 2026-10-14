#!/bin/bash
# Runs scripts/steamdeck/rebuild-check.sh against a stub box: no Deck, no build.
#
#   a host binary that loads         -> exit 0, update.sh never runs
#   no host binary                   -> update.sh runs once and the attempt stamp is cleared
#   the same with no host.env        -> update.sh still runs
#   update.sh fails                  -> the stamp stays, and the next boot does not retry
#
# bash -n and shellcheck cannot see an unbound variable in lib.sh or a broken `set -e` path; this
# sources the real lib.sh and takes each branch. update.sh is a stub that logs its calls.
set -euo pipefail
cd "$(dirname "$0")/../.."
repo=$PWD
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
src=$tmp/src
host=$src/target-steamos/release/punktfunk-host
mkdir -p "$src/scripts/steamdeck" "$(dirname "$host")" "$tmp/home/.config/punktfunk"
: > "$tmp/home/.config/punktfunk/host.env" # install.sh always writes one

stub() {  # stub <exit status>: an update.sh that logs a call and exits with it
    printf '#!/bin/sh\necho ran >> "%s/calls"\nexit %s\n' "$tmp" "$1" > "$src/scripts/steamdeck/update.sh"
}
run() { HOME=$tmp/home PUNKTFUNK_SRC=$src bash "$repo/scripts/steamdeck/rebuild-check.sh" 2>&1; }
calls() { if [ -f "$tmp/calls" ]; then wc -l < "$tmp/calls" | tr -d ' '; else echo 0; fi; }
stamp=$tmp/home/.cache/punktfunk/rebuild-attempt
fail() { echo "check-steamdeck-rebuild: $*" >&2; exit 1; }

stub 0
cp "$(type -P true)" "$host"
run > /dev/null
[ "$(calls)" = 0 ] || fail "a loadable host binary still ran update.sh"

rm "$host"
run > /dev/null
[ "$(calls)" = 1 ] || fail "a missing host binary ran update.sh $(calls) times, not once"
[ ! -e "$stamp" ] || fail "a successful rebuild left its attempt stamp"

rm "$tmp/home/.config/punktfunk/host.env"
run > /dev/null || fail "with no host.env a missing host binary did not rebuild"
[ "$(calls)" = 2 ] || fail "with no host.env update.sh ran $(calls) times in all, not twice"
: > "$tmp/home/.config/punktfunk/host.env"

stub 1
run > /dev/null && fail "a failed update.sh did not fail the check"
[ -e "$stamp" ] || fail "a failed rebuild cleared its attempt stamp"
out=$(run)
case $out in *"not retrying"*) ;; *) fail "the second boot retried the rebuild: $out" ;; esac
[ "$(calls)" = 3 ] || fail "the second boot ran update.sh again"
echo "check-steamdeck-rebuild: healthy, rebuild and no-retry paths behave"
