# Windows' hook.sh: hands one Claude Code hook event (JSON on stdin) to
# littleguys and prints its answer. littleguys listens on a port on 127.0.0.1
# and only answers requests that carry its secret; each time it starts it writes
# "<port> <secret> <its process id>" to `endpoint`, next to this file.
#
# Claude Code runs it in PowerShell whether or not Git Bash is installed (the
# Windows hooks.json says so), read in as a script block, so the execution
# policy doesn't stop it. Whatever happens it exits 0 and, when littleguys isn't
# running, prints nothing at once: Claude Code goes on as if the hook weren't
# there. A PermissionRequest hook that fails in PowerShell would block it.
#
#   hook.ps1 <Event> [max seconds]
param([string]$HookEvent, [int]$Seconds = 10)
try {
  $endpoint = [IO.File]::ReadAllText((Join-Path $env:CLAUDE_PLUGIN_ROOT 'hooks\endpoint')).Trim() -split ' '
  $port, $secret, $owner = $endpoint
  # A refused connection takes Windows seconds; a gone process is quick to see.
  if (-not $owner -or -not (Get-Process -Id ([int]$owner) -ErrorAction SilentlyContinue)) { exit 0 }

  $in = New-Object IO.MemoryStream
  [Console]::OpenStandardInput().CopyTo($in)
  $body = $in.ToArray()

  $request = [Net.HttpWebRequest]::Create("http://127.0.0.1:$port/hook/$HookEvent")
  $request.Method = 'POST'
  $request.Proxy = $null
  $request.Timeout = $Seconds * 1000
  $request.ReadWriteTimeout = $Seconds * 1000
  $request.ServicePoint.Expect100Continue = $false
  $request.ContentType = 'application/json'
  $request.Headers.Add('X-Littleguys-Secret', $secret)
  if ($env:CLAUDE_CODE_ENTRYPOINT) { $request.Headers.Add('X-Claude-Entrypoint', $env:CLAUDE_CODE_ENTRYPOINT) }
  if ($env:CLAUDE_JOB_DIR) { $request.Headers.Add('X-Claude-Background', '1') }
  $request.ContentLength = $body.Length
  $sent = $request.GetRequestStream()
  $sent.Write($body, 0, $body.Length)
  $sent.Close()

  $response = $request.GetResponse()
  $answer = New-Object IO.MemoryStream
  $response.GetResponseStream().CopyTo($answer)
  $response.Close()
  $bytes = $answer.ToArray()
  $out = [Console]::OpenStandardOutput()
  $out.Write($bytes, 0, $bytes.Length)
  $out.Flush()
} catch {}
exit 0
