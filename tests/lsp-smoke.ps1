$ErrorActionPreference = 'Stop'
$exe = "d:\WorkGit\my-angel-script-lsp\lsp\target\debug\as-lsp.exe"

$psi = [System.Diagnostics.ProcessStartInfo]::new()
$psi.FileName = $exe
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false

$p = [System.Diagnostics.Process]::Start($psi)
Start-Sleep -Milliseconds 500

function Send([string]$body) {
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($body)
    $header = [System.Text.Encoding]::ASCII.GetBytes("Content-Length: $($bytes.Length)`r`n`r`n")
    $all = New-Object byte[] ($header.Length + $bytes.Length)
    [Array]::Copy($header, 0, $all, 0, $header.Length)
    [Array]::Copy($bytes, 0, $all, $header.Length, $bytes.Length)
    $stream = $p.StandardInput.BaseStream
    $stream.Write($all, 0, $all.Length)
    $stream.Flush()
}

Send '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{},"processId":null,"rootUri":null}}'
Start-Sleep -Milliseconds 800
Send '{"jsonrpc":"2.0","method":"initialized","params":{}}'
Start-Sleep -Milliseconds 300
Send '{"jsonrpc":"2.0","method":"exit"}'

if (-not $p.WaitForExit(3000)) { $p.Kill() }
$out = $p.StandardOutput.ReadToEnd()
$err = $p.StandardError.ReadToEnd()

if ($out -match 'my-as-lsp' -and $out -match 'documentSymbolProvider') {
    Write-Output "SMOKE OK: initialize response contains serverInfo + capabilities"
    if ($out -match '"name":"my-as-lsp"') { Write-Output "  serverInfo: my-as-lsp" }
    if ($out -match 'as_typename') {
        Write-Output "  semanticTokens legend: as_typename present"
    } else {
        $i = $out.IndexOf('semanticTokensProvider')
        if ($i -ge 0) { Write-Output ("  ctx: " + $out.Substring($i, 200)) }
    }
} else {
    Write-Output "SMOKE FAIL"
    Write-Output ("exit code: " + $p.ExitCode)
    Write-Output "--- stdout ---"
    Write-Output $out.Substring(0, [Math]::Min(2000, $out.Length))
    Write-Output "--- stderr ---"
    Write-Output $err.Substring(0, [Math]::Min(1000, $err.Length))
}
