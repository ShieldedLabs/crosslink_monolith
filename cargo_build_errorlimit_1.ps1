# cargo_build_filter.ps1
# Streams cargo build output in real-time, showing only the first error.
# Kills the cargo process once a second error line appears (no wasted compile time).

$cargoArgs = "build --color always"
if ($args.Count -gt 0) {
    $cargoArgs = "build --color always " + ($args -join " ")
}

$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = "cargo"
$psi.Arguments = $cargoArgs
$psi.RedirectStandardError  = $true
$psi.RedirectStandardOutput = $false   # stdout passes straight through
$psi.UseShellExecute        = $false

$proc = [System.Diagnostics.Process]::Start($psi)

$errorCount = 0
# Strip ANSI escapes for matching so color codes don't interfere with the regex
$ansiPattern = '\x1b\[[0-9;]*m'

while ($null -ne ($line = $proc.StandardError.ReadLine())) {
    $plain = $line -replace $ansiPattern, ''
    if ($plain -match '^error') {
        $errorCount++
    }
    if ($errorCount -ge 2) {
        # First error fully printed; kill cargo + any child rustc processes
        try {
            $children = Get-CimInstance Win32_Process -Filter "ParentProcessId=$($proc.Id)" -ErrorAction SilentlyContinue
            foreach ($child in $children) {
                Write-Host "Killing $($child.ProcessId)"
                Stop-Process -Id $child.ProcessId -Force -ErrorAction SilentlyContinue
            }
            $proc.Kill()
        } catch {}
        break
    }

    # Print raw line so ANSI escape sequences survive
    [Console]::Error.WriteLine($line)
}

$proc.WaitForExit()

# If we killed it, return failure (1); otherwise return cargo's real exit code
if ($errorCount -ge 2) {
    exit 1
} else {
    exit $proc.ExitCode
}
