param(
    [int]$Jobs = 4,
    [string]$Toolchain = 'stable',
    [string]$TargetDirectory = 'target/storage-probe'
)

$ErrorActionPreference = 'Stop'
$probeRoot = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$probeManifest = Join-Path $PSScriptRoot 'Cargo.toml'
$probeTarget = Join-Path $probeRoot $TargetDirectory
$probeOldPath = $env:PATH
$probeOldProtoc = $env:PROTOC

try {
    if (-not (Get-Command cmake -ErrorAction SilentlyContinue) -or
        -not (Get-Command ninja -ErrorAction SilentlyContinue)) {
        $probeVswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
        $probeVs = & $probeVswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
        if (-not $probeVs) { throw 'MSVC C++ build tools are required for the kuzu crate.' }
        $env:PATH = "$probeVs/Common7/IDE/CommonExtensions/Microsoft/CMake/CMake/bin;$probeVs/Common7/IDE/CommonExtensions/Microsoft/CMake/Ninja;$env:PATH"
    }
    $probeProtoc = & cargo "+$Toolchain" run --manifest-path $probeManifest --locked --offline --no-default-features --features build-tools --bin probe-protoc --target-dir $probeTarget -j $Jobs
    if ($LASTEXITCODE -ne 0) { throw 'Failed to locate the vendored protoc executable.' }
    $env:PROTOC = $probeProtoc.Trim()
    & cargo "+$Toolchain" build --manifest-path $probeManifest --locked --offline --target-dir $probeTarget -j $Jobs
    if ($LASTEXITCODE -ne 0) { throw 'Rust storage probe build failed.' }
} finally {
    $env:PATH = $probeOldPath
    $env:PROTOC = $probeOldProtoc
}
