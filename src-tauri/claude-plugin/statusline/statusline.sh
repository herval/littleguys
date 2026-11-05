#!/bin/sh
# Claude Code's status line while "Show usage limits" is on in littleguys. Claude
# Code tells status lines how much of its limits you've used, and nothing else,
# so this hands littleguys what it was given, then runs your own status line
# (own.sh, next to this file) with the same input and prints what that prints.
# littleguys keeps only the limits. Turning the setting off puts your status line
# back. littleguys' socket is in the folder above this one.
dir=${0%/*}
sock=${dir%/*}/claude.sock
input=$(cat; printf x)
input=${input%x}
if [ -S "$sock" ]; then
  # In the background, so littleguys can never slow your status line down.
  printf '%s' "$input" | curl -sf --max-time 2 --unix-socket "$sock" \
    -H 'Content-Type: application/json' -H 'Expect:' \
    --data-binary @- http://littleguys/limits/claude >/dev/null 2>&1 &
fi
[ -f "$dir/own.sh" ] || exit 0
printf '%s' "$input" | sh "$dir/own.sh"
