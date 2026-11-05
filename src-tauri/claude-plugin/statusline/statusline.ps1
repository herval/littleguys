# Windows' statusline.sh: Claude Code's status line while "Show usage limits" is
# on in littleguys. Claude Code tells status lines how much of its limits you've
# used, and nothing else, so this hands littleguys what it was given, then runs
# your own status line (own.ps1, next to this file) with the same input and prints
# what that prints. littleguys keeps only the limits. Turning the setting off puts
# your status line back.
#
# Claude Code runs status lines in Git Bash when it's installed, else in
# PowerShell, so your own runs in the same one. littleguys' port and secret are in
# the plugin's `endpoint` file (see hook.ps1).
$in = New-Object IO.MemoryStream
[Console]::OpenStandardInput().CopyTo($in)
$body = $in.ToArray()

try {
  $endpoint = Join-Path (Split-Path $PSScriptRoot) 'claude-plugin\plugin\hooks\endpoint'
  $port, $secret, $owner = [IO.File]::ReadAllText($endpoint).Trim() -split ' '
  if ($owner -and (Get-Process -Id ([int]$owner) -ErrorAction SilentlyContinue)) {
    $request = [Net.HttpWebRequest]::Create("http://127.0.0.1:$port/limits/claude")
    $request.Method = 'POST'
    $request.Proxy = $null
    $request.Timeout = 1000
    $request.ReadWriteTimeout = 1000
    $request.ServicePoint.Expect100Continue = $false
    $request.ContentType = 'application/json'
    $request.Headers.Add('X-Littleguys-Secret', $secret)
    $request.ContentLength = $body.Length
    $sent = $request.GetRequestStream()
    $sent.Write($body, 0, $body.Length)
    $sent.Close()
    $request.GetResponse().Close()
  }
} catch {}

$own = Join-Path $PSScriptRoot 'own.ps1'
if (-not (Test-Path $own)) { exit 0 }

$bash = $env:CLAUDE_CODE_GIT_BASH_PATH
if (-not $bash) {
  $git = Get-Command git.exe -ErrorAction SilentlyContinue
  if ($git) { $bash = Join-Path (Split-Path (Split-Path $git.Source)) 'bin\bash.exe' }
}
$start = New-Object Diagnostics.ProcessStartInfo
if ($bash -and (Test-Path $bash)) {
  $start.FileName = $bash
  $start.Arguments = "`"$own`""
} else {
  $start.FileName = 'powershell.exe'
  $start.Arguments = "-NoProfile -ExecutionPolicy Bypass -File `"$own`""
}
$start.UseShellExecute = $false
$start.RedirectStandardInput = $true
$process = [Diagnostics.Process]::Start($start)
$process.StandardInput.BaseStream.Write($body, 0, $body.Length)
$process.StandardInput.Close()
$process.WaitForExit()
exit $process.ExitCode
