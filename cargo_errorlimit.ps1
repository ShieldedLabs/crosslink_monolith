# cargo_errorlimit.ps1
# Streams cargo output in real-time, showing only the first error.
# Kills the cargo process once a second error line appears (no wasted compile time).
#
# Usage: cargo_errorlimit.ps1 <subcommand> [cargo args...]   e.g. build / test

if ($args.Count -lt 1) {
    [Console]::Error.WriteLine("cargo_errorlimit.ps1: missing cargo subcommand (e.g. build or test)")
    exit 1
}

# First arg is the cargo subcommand (build/test/...); the rest are cargo args.
$cargoArgs = $args -join " "

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
