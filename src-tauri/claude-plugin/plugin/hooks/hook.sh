#!/bin/sh
# Hands one Claude Code hook event (JSON on stdin) to littleguys and prints its
# answer. On macOS and Linux littleguys listens on a unix socket, so only this
# user can reach it. Windows has no unix sockets for this, so there it listens
# on a port on 127.0.0.1 and only answers requests carrying the secret below.
# littleguys writes these values when it installs the plugin and when it starts.
# When littleguys isn't running this does nothing, at once: Claude Code goes on
# as if the hook weren't there.
#
#   hook.sh <Event> [max seconds]
sock='__LITTLEGUYS_SOCKET__'
port='__LITTLEGUYS_PORT__'
secret='__LITTLEGUYS_SECRET__'
if [ -n "$port" ]; then
  via=''
  url="http://127.0.0.1:$port/hook/$1"
elif [ -S "$sock" ]; then
  via='--unix-socket'
  url="http://littleguys/hook/$1"
else
  cat >/dev/null
  exit 0
fi
curl -sf --max-time "${2:-10}" ${via:+"$via" "$sock"} \
  -H 'Content-Type: application/json' -H 'Expect:' \
  -H "X-Littleguys-Secret: $secret" \
  -H "X-Claude-Entrypoint: ${CLAUDE_CODE_ENTRYPOINT:-}" \
  -H "X-Claude-Background: ${CLAUDE_JOB_DIR:+1}" \
  --data-binary @- "$url" 2>/dev/null
exit 0
