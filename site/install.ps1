# Installs ra-jit, cargo-ra-jit and ra-jit-rustc from a GitHub release.
#
#   irm https://dioxuslabs.github.io/rustd/install.ps1 | iex
#
# $env:RA_JIT_VERSION names the release (`latest` unless set); $env:RA_JIT_INSTALL_DIR is where
# the three binaries go (cargo's bin directory unless set).
$ErrorActionPreference = "Stop"

$repo = "https://github.com/DioxusLabs/rustd"
$version = if ($env:RA_JIT_VERSION) { $env:RA_JIT_VERSION } else { "latest" }
$cargo = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $HOME ".cargo" }
$dir = if ($env:RA_JIT_INSTALL_DIR) { $env:RA_JIT_INSTALL_DIR } else { Join-Path $cargo "bin" }
$from = if ($version -eq "latest") { "$repo/releases/latest/download" } else { "$repo/releases/download/$version" }
$archive = "ra-jit-x86_64-pc-windows-msvc.zip"

$work = Join-Path ([IO.Path]::GetTempPath()) ("ra-jit-" + [Guid]::NewGuid())
New-Item -ItemType Directory $work | Out-Null
try {
    Write-Host "ra-jit: downloading $from/$archive"
    try {
        Invoke-WebRequest "$from/$archive" -OutFile (Join-Path $work $archive)
    } catch {
        if ($version -ne "latest") { throw }
        # Before the first numbered release there is only the build of main.
        $from = "$repo/releases/download/nightly"
        Write-Host "ra-jit: there is no numbered release yet; downloading $from/$archive"
        Invoke-WebRequest "$from/$archive" -OutFile (Join-Path $work $archive)
    }
    Invoke-WebRequest "$from/SHA256SUMS" -OutFile (Join-Path $work "SHA256SUMS")
    $want = (Select-String -Path (Join-Path $work "SHA256SUMS") -Pattern ([regex]::Escape($archive))).Line.Split(" ")[0]
    $got = (Get-FileHash (Join-Path $work $archive) -Algorithm SHA256).Hash
    if (-not $want -or $want -ne $got.ToLower()) { throw "the checksum of $archive is not the release's" }
    Expand-Archive (Join-Path $work $archive) -DestinationPath $work
    New-Item -ItemType Directory $dir -Force | Out-Null
    Copy-Item (Join-Path $work "ra-jit-x86_64-pc-windows-msvc\*.exe") $dir -Force
    Write-Host "ra-jit: installed ra-jit, cargo-ra-jit and ra-jit-rustc in $dir"
    Write-Host "ra-jit: it needs Rust 1.99.0 with rust-src: rustup toolchain install 1.99.0 --component rust-src"
} finally {
    Remove-Item $work -Recurse -Force
}
