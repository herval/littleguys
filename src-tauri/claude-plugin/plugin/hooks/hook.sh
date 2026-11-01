#!/bin/sh
# Hands one Claude Code hook event (JSON on stdin) to littleguys over its unix
# socket, which only this user can open, and prints its answer. littleguys
# writes the socket's path here when it installs the plugin and when it starts.
# When littleguys isn't running this does nothing, at once: Claude Code goes on
# as if the hook weren't there. (On Windows the hooks run hook.ps1 instead.)
#
#   hook.sh <Event> [max seconds]
sock='__LITTLEGUYS_SOCKET__'
if [ ! -S "$sock" ]; then
  cat >/dev/null
  exit 0
fi
curl -sf --max-time "${2:-10}" --unix-socket "$sock" \
  -H 'Content-Type: application/json' -H 'Expect:' \
  -H "X-Claude-Entrypoint: ${CLAUDE_CODE_ENTRYPOINT:-}" \
  -H "X-Claude-Background: ${CLAUDE_JOB_DIR:+1}" \
  --data-binary @- "http://littleguys/hook/$1" 2>/dev/null
exit 0
