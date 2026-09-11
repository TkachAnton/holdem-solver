# ============================================================
#  make-bundle.ps1  --  pack a project into ONE plain-text file
# ============================================================
# Why a .txt bundle instead of a zip:
#   * a plain text attachment always uploads;
#   * every file is verified by sha256 (whole file AND per line),
#     so a truncated or mangled upload is detected, not guessed at.
#
# Usage (PowerShell, from the project root):
#     powershell -ExecutionPolicy Bypass -File tools\make-bundle.ps1
#
# Optional:
#     -OutPath D:\bundle.txt   where to write the bundle
#     -MaxBytes 800000         max characters per file part
# ============================================================

param(
    [string]$OutPath = "",
    [int]$MaxBytes = 800000,
    [string]$Root = "."
)

$ErrorActionPreference = "Stop"

if ([string]::IsNullOrWhiteSpace($OutPath)) {
    $OutPath = Join-Path ([System.IO.Path]::GetTempPath()) "holdem-bundle.txt"
}

$rootFull = (Resolve-Path $Root).Path

# Directories that are never packed.
$skipDirs = @(
    'target', '.git', 'node_modules', '.venv', 'venv', '__pycache__',
    '.cache', 'dist', 'build', 'out', '.idea', '.vs', '.vscode',
    'checkpoints', 'jobs', 'tools'
)

# File names / extensions we keep.
$keepExt = @('.rs', '.toml', '.json', '.md', '.html', '.js', '.mjs', '.cjs',
             '.ts', '.tsx', '.css', '.txt', '.yml', '.yaml', '.sh', '.ps1',
             '.bat', '.cmd', '.csv', '.sql', '.lock', '.editorconfig',
             '.gitignore', '.env.example')

$keepNames = @('makefile', 'dockerfile', 'rust-toolchain', 'rust-toolchain.toml',
               '.gitignore', '.editorconfig', 'cargo.lock')

Write-Host ""
Write-Host "Project root : $rootFull"
Write-Host "Bundle file  : $OutPath"
Write-Host ""

$skipSet = [System.Collections.Generic.HashSet[string]]::new([string[]]$skipDirs)
$keepExtSet = [System.Collections.Generic.HashSet[string]]::new([string[]]$keepExt)
$keepNameSet = [System.Collections.Generic.HashSet[string]]::new([string[]]$keepNames)

$files = New-Object System.Collections.Generic.List[object]
$skipped = New-Object System.Collections.Generic.List[string]

foreach ($f in (Get-ChildItem -Path $rootFull -Recurse -File -Force -ErrorAction SilentlyContinue)) {
    $rel = $f.FullName.Substring($rootFull.Length).TrimStart('\', '/')
    $rel = $rel -replace '\\', '/'

    $segs = $rel.Split('/')
    $skip = $false
    for ($k = 0; $k -lt $segs.Length - 1; $k++) {
        if ($skipSet.Contains($segs[$k])) { $skip = $true; break }
    }
    if ($skip) { continue }

    $ext = [System.IO.Path]::GetExtension($f.Name).ToLowerInvariant()
    $nameL = $f.Name.ToLowerInvariant()
    if (-not ($keepExtSet.Contains($ext) -or $keepNameSet.Contains($nameL))) {
        $skipped.Add($rel)
        continue
    }

    $files.Add([pscustomobject]@{ Path = $rel; Full = $f.FullName; Size = $f.Length })
}

$files = $files | Sort-Object Path

$utf8Strict = New-Object System.Text.UTF8Encoding($false, $true)
$utf8Plain = New-Object System.Text.UTF8Encoding($false)
$sha = [System.Security.Cryptography.SHA256]::Create()

function Get-Hex([byte[]]$b) {
    return ([System.BitConverter]::ToString($sha.ComputeHash($b))).Replace('-', '').ToLowerInvariant()
}

# ---------- pass 1: read + hash every file ----------
$entries = New-Object System.Collections.Generic.List[object]
foreach ($f in $files) {
    $bytes = [System.IO.File]::ReadAllBytes($f.Full)
    $text = $null
    try { $text = $utf8Strict.GetString($bytes) }
    catch { $skipped.Add($f.Path + "  [not valid UTF-8]"); continue }

    $crlf = $text.Contains("`r`n")
    $lf = $text -replace "`r`n", "`n"
    $trimmed = $lf
    $endsnl = $trimmed.EndsWith("`n")
    if ($endsnl) { $trimmed = $trimmed.Substring(0, $trimmed.Length - 1) }

    if ($trimmed.Length -eq 0) { $lineArr = @() } else { $lineArr = $trimmed -split "`n" }
    $hashes = New-Object System.Collections.Generic.List[string]
    foreach ($ln in $lineArr) { $hashes.Add((Get-Hex ($utf8Plain.GetBytes($ln)))) }

    $entries.Add([pscustomobject]@{
        Path    = $f.Path
        Text    = $trimmed
        Sha     = (Get-Hex ($utf8Plain.GetBytes($trimmed)))
        ShaRaw  = (Get-Hex $bytes)
        Bytes   = $bytes.Length
        BytesLf = $utf8Plain.GetByteCount($trimmed)
        Lines   = $lineArr.Count
        Endsnl  = $endsnl
        Crlf    = $crlf
        Hashes  = ($hashes -join ',')
    })
}

# ---------- pass 2: write the bundle ----------
$fs = New-Object System.IO.FileStream($OutPath, [System.IO.FileMode]::Create, [System.IO.FileAccess]::Write)
$sw = New-Object System.IO.StreamWriter($fs, $utf8Plain)
$sw.NewLine = "`n"

$sw.WriteLine("### HOLD-SOLVER BUNDLE v4")
$sw.WriteLine("### generated: " + (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'))
$sw.WriteLine("### root: " + $rootFull)
$sw.WriteLine("### files: " + $entries.Count)
$sw.WriteLine("### max-bytes-per-part: " + $MaxBytes)
$sw.WriteLine("### --- MANIFEST ---")
$sw.WriteLine("### line 1 format: sha256_lf | chars | bytes_lf | lines | crlf | ends_with_newline | sha256_raw | path")
$sw.WriteLine("### line 2 format: ###L <comma separated sha256, one per line>")

$totalBytes = 0
foreach ($e in $entries) {
    $sw.WriteLine("### " + $e.Sha + " | " + $e.Text.Length + " | " + $e.BytesLf + " | " + $e.Lines + " | " + $e.Crlf.ToString().ToLowerInvariant() + " | " + $e.Endsnl.ToString().ToLowerInvariant() + " | " + $e.ShaRaw + " | " + $e.Path)
    $sw.WriteLine("###L " + $e.Hashes)
    $totalBytes += $e.Bytes
}
$sw.WriteLine("### --- END MANIFEST ---")
$sw.WriteLine("")

$partCount = 0
foreach ($e in $entries) {
    if ($e.Text.Length -le $MaxBytes) {
        $sw.WriteLine("=== BEGIN " + $e.Path + " " + $e.Text.Length + " ===")
        $sw.Write($e.Text)
        $sw.WriteLine("")
        $sw.WriteLine("=== END " + $e.Path + " ===")
        $sw.WriteLine("")
        $partCount++
    }
    else {
        $n = [Math]::Ceiling($e.Text.Length / $MaxBytes)
        for ($i = 0; $i -lt $n; $i++) {
            $len = [Math]::Min($MaxBytes, $e.Text.Length - ($i * $MaxBytes))
            $sw.WriteLine("=== BEGIN " + $e.Path + " [part " + ($i + 1) + "/" + $n + "] " + $len + " ===")
            $sw.Write($e.Text.Substring($i * $MaxBytes, $len))
            $sw.WriteLine("")
            $sw.WriteLine("=== END " + $e.Path + " [part " + ($i + 1) + "/" + $n + "] ===")
            $sw.WriteLine("")
            $partCount++
        }
    }
}

$sw.Flush()
$sw.Close()

$size = (Get-Item $OutPath).Length
Write-Host "---------------------------------------------"
Write-Host ("Packed files  : " + $entries.Count)
Write-Host ("Skipped files : " + $skipped.Count)
Write-Host ("Source bytes  : " + $totalBytes)
Write-Host ("Sections      : " + $partCount)
Write-Host ("Bundle size   : " + [Math]::Round($size / 1KB, 1) + " KB")
Write-Host "---------------------------------------------"
Write-Host ""
Write-Host ">>> ATTACH THIS FILE TO THE CHAT:"
Write-Host ("    " + $OutPath)
Write-Host ""
Write-Host "To reveal it in Explorer, run:"
Write-Host ("    explorer /select,`"" + $OutPath + "`"")
Write-Host ""

if ($skipped.Count -gt 0) {
    Write-Host ("Skipped (binary/unknown), first 20 of " + $skipped.Count + ":")
    $skipped | Select-Object -First 20 | ForEach-Object { Write-Host ("  - " + $_) }
}
