$ErrorActionPreference = "Stop"
$Vendor = Join-Path (Split-Path -Parent $PSScriptRoot) "vendor/llama.cpp"
Push-Location $Vendor
try {
    Write-Host "Configuring llama.cpp (Release)..."
    # llama-cli links the llama-server-impl library, so the server target must be enabled.
    # MinGW's headers default to Windows 7 (_WIN32_WINNT=0x0601), and llama.cpp's bundled HTTP library
    # refuses to compile below Windows 10 ("cpp-httplib doesn't support Windows 8 or lower"). Tell the
    # compiler to target Windows 10 or later.
    # Aqua talks to llama-server over HTTP and falls back to llama-cli. The embedded web page,
    # examples, tests, and the unified app binary are not used.
    cmake -S . -B build `
        -DLLAMA_BUILD_SERVER=ON `
        -DLLAMA_BUILD_UI=OFF `
        -DLLAMA_BUILD_EXAMPLES=OFF `
        -DLLAMA_BUILD_TESTS=OFF `
        -DLLAMA_BUILD_APP=OFF `
        "-DCMAKE_C_FLAGS=-D_WIN32_WINNT=0x0A00" `
        "-DCMAKE_CXX_FLAGS=-D_WIN32_WINNT=0x0A00"
    Write-Host "Building llama-cli..."
    cmake --build build --config Release --target llama-cli
    Write-Host "Building llama-server (keeps the model and prompt cache between agent steps)..."
    cmake --build build --config Release --target llama-server
    & (Join-Path $PSScriptRoot "bundle_llama_dlls.ps1")
    Write-Host "Expected binary: $Vendor/build/bin/Release/llama-cli.exe"
} finally {
    Pop-Location
}
