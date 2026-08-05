# cargo_errorlimit.ps1
# Streams cargo output in real-time, showing only the first error.
# Kills the cargo process once a second error line appears (no wasted compile time).
#
# Usage: cargo_errorlimit.ps1 <exe> <subcommand> [cargo args...]   e.g. cargo build
#                                                                      wsl -e ~/.cargo/bin/cargo test

if ($args.Count -lt 2) {
    [Console]::Error.WriteLine("cargo_errorlimit.ps1: usage: <exe> <cargo subcommand> [cargo args...]")
    exit 1
}

# Every descendant of $rootId, deepest first, from a single snapshot walked in memory.
# rustc spawns the linker, so stopping at direct children leaves link.exe running.
# Nothing that started before the root can be its descendant, and that check is what
# keeps a recycled PID from ever pulling an unrelated process into the walk.
function Get-Descendants([int]$rootId) {
    $all  = @(Get-CimInstance Win32_Process -ErrorAction SilentlyContinue)
    $root = $all | Where-Object { $_.ProcessId -eq $rootId } | Select-Object -First 1
    if ($null -eq $root) { return @() }

    $byParent = @{}
    foreach ($p in $all) {
        if ($null -eq $p.CreationDate -or $p.CreationDate -lt $root.CreationDate) { continue }
        $key = [int]$p.ParentProcessId
        if (-not $byParent.ContainsKey($key)) { $byParent[$key] = @() }
        $byParent[$key] += [int]$p.ProcessId
    }

    $ordered  = @()
    $seen     = @{ $rootId = $true }
    $frontier = @($rootId)

    while ($frontier.Count -gt 0) {
        $next = @()
        foreach ($id in $frontier) {
            foreach ($child in $byParent[$id]) {
                if ($seen.ContainsKey($child)) { continue }
                $seen[$child] = $true
                $ordered += $child
                $next    += $child
            }
        }
        $frontier = $next
    }

    [array]::Reverse($ordered)
    return $ordered
}

# First arg is the program to launch: cargo directly, or wsl.exe relaying to a Linux
# cargo. Everything after it is that program's arguments.
$exe = $args[0]
$cargoArgs = ($args | Select-Object -Skip 1) -join " "

$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $exe
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
        # First error fully printed; kill cargo and everything below it. The tree has to be
        # collected before the root dies, because orphaned children keep a ParentProcessId
        # that no longer leads back to it.
        # Under WSL there are no Win32 descendants to find, but killing the wsl.exe relay
        # tears down the whole Linux process tree, rustc included.
        $doomed = @()
        try { $doomed = @(Get-Descendants $proc.Id) } catch {}
        try { $proc.Kill() } catch {}
        foreach ($id in $doomed) {
            Write-Host "Killing $id"
            Stop-Process -Id $id -Force -ErrorAction SilentlyContinue
        }
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
