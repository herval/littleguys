#!/bin/sh
# Hands one Claude Code hook event (JSON on stdin) to littleguys and prints its
# answer. littleguys listens on a unix socket, so only this user can reach it,
# and writes the socket's path below when it installs the plugin.
# When littleguys isn't running this does nothing, at once: Claude Code goes on
# as if the hook weren't there.
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
