<#
.SYNOPSIS
    herd-tune -- Detect GPU/VRAM/RAM, configure backend, and register with Herd.
.DESCRIPTION
    Run on any Windows machine to auto-detect hardware, set up either Ollama or
    llama-server as the inference backend, and register with a Herd instance.
.PARAMETER Apply
    Apply recommended OLLAMA_* environment variables and restart the Ollama service.
    Requires administrator privileges. Only used with Ollama backend.
.PARAMETER Herd
    Override the Herd endpoint URL (default: baked in at download time).
.PARAMETER Backend
    Backend type: ollama, llama-server, auto (default: auto).
    auto: if Ollama is running on :11434, use it; otherwise set up llama-server.
.PARAMETER Port
    llama-server listen port (default: 8090).
.PARAMETER Context
    llama-server context length (default: 4096, auto-adjusted by VRAM).
.PARAMETER Model
    Path to GGUF model file for llama-server.
.PARAMETER EnrollmentKey
    Enrollment key for node registration.
.PARAMETER Daemon
    Keep herd-tune resident with HTTP control API (stretch goal, not yet implemented).
#>
[CmdletBinding()]
param(
    [switch]$Apply,
    [string]$Herd,
    [ValidateSet("ollama", "llama-server", "auto")]
    [string]$Backend = "auto",
    [int]$Port = 8090,
    [int]$Context = 4096,
    [string]$Model = "",
    [string]$EnrollmentKey,
    [switch]$Daemon
)

# -- Herd Registration (auto-configured on download) --
$HerdEndpoint = "%%HERD_ENDPOINT%%"
$BakedEnrollmentKey = "%%ENROLLMENT_KEY%%"
$HerdTuneVersion = "1.0.0"

# -Herd parameter overrides the baked-in endpoint.
# Also check HERD_URL env var as fallback (useful for containers/CI).
if ($Herd) {
    $HerdEndpoint = $Herd
} elseif ($env:HERD_URL) {
    $HerdEndpoint = $env:HERD_URL
}

# -EnrollmentKey parameter overrides baked-in key
if ($EnrollmentKey) {
    $BakedEnrollmentKey = $EnrollmentKey
}

# -- Require admin only when -Apply is used --
if ($Apply) {
    $currentPrincipal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
    if (-not $currentPrincipal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        # Re-launch elevated, forwarding arguments
        $argList = "-ExecutionPolicy Bypass -File `"$PSCommandPath`" -Apply -Backend `"$Backend`""
        if ($HerdEndpoint -and $HerdEndpoint -ne '%%HERD_ENDPOINT%%') {
            $argList += " -Herd `"$HerdEndpoint`""
        }
        if ($BakedEnrollmentKey -and $BakedEnrollmentKey -ne '%%ENROLLMENT_KEY%%') {
            $argList += " -EnrollmentKey `"$BakedEnrollmentKey`""
        }
        if ($Port -ne 8090) { $argList += " -Port $Port" }
        if ($Context -ne 4096) { $argList += " -Context $Context" }
        if ($Model) { $argList += " -Model `"$Model`"" }
        Start-Process powershell.exe -Verb RunAs -ArgumentList $argList -Wait
        exit $LASTEXITCODE
    }
}

# ======================================================================
# GPU VENDOR DETECTION
# ======================================================================

function Get-GpuDetailed {
    $result = @{
        Vendor          = "none"
        Model           = ""
        Backend         = "cpu"
        DriverVersion   = ""
        CudaVersion     = ""
        ComputeCap      = ""
        CudaMajor       = 0
        VramMb          = 0
        IsBlackwell     = $false
    }

    # -- Try NVIDIA first --
    try {
        $smiOutput = & nvidia-smi --query-gpu=name,memory.total,driver_version --format=csv,noheader,nounits 2>$null
        if ($LASTEXITCODE -eq 0 -and $smiOutput) {
            $parts = $smiOutput.Split(',').Trim()
            $result.Vendor = "nvidia"
            $result.Model = $parts[0]
            $result.VramMb = [int]$parts[1]
            $result.DriverVersion = $parts[2]
            $result.Backend = "cuda"

            # Get compute capability
            try {
                $ccOutput = & nvidia-smi --query-gpu=compute_cap --format=csv,noheader 2>$null
                if ($LASTEXITCODE -eq 0 -and $ccOutput) {
                    $result.ComputeCap = $ccOutput.Trim()
                }
            } catch {}

            # Get CUDA version from nvidia-smi header
            try {
                $smiHeader = & nvidia-smi 2>$null
                if ($smiHeader) {
                    $headerText = $smiHeader | Out-String
                    if ($headerText -match 'CUDA Version:\s*([\d.]+)') {
                        $result.CudaVersion = $Matches[1]
                    }
                }
            } catch {}

            # Determine CUDA major version based on compute capability
            # Blackwell (RTX 5000-series) has compute capability >= 12.0
            if ($result.ComputeCap) {
                $ccMajor = [int]($result.ComputeCap.Split('.')[0])
                if ($ccMajor -ge 12) {
                    $result.CudaMajor = 13
                    $result.IsBlackwell = $true
                } else {
                    $result.CudaMajor = 12
                }
            } else {
                $result.CudaMajor = 12
            }

            return $result
        }
    } catch {}

    # -- Try AMD (WMI-based on Windows) --
    try {
        $amdGpu = Get-CimInstance -ClassName Win32_VideoController | Where-Object {
            $_.Name -match 'AMD|Radeon'
        } | Sort-Object AdapterRAM -Descending | Select-Object -First 1

        if ($amdGpu) {
            $result.Vendor = "amd"
            $result.Model = $amdGpu.Name
            $result.Backend = "rocm"
            $result.VramMb = [math]::Round($amdGpu.AdapterRAM / 1MB)
            $result.DriverVersion = $amdGpu.DriverVersion
            return $result
        }
    } catch {}

    # -- Try Intel (WMI for Arc GPUs) --
    try {
        $intelGpu = Get-CimInstance -ClassName Win32_VideoController | Where-Object {
            $_.Name -match 'Intel.*Arc|Intel.*Xe'
        } | Sort-Object AdapterRAM -Descending | Select-Object -First 1

        if ($intelGpu) {
            $result.Vendor = "intel"
            $result.Model = $intelGpu.Name
            $result.Backend = "sycl"
            $result.VramMb = [math]::Round($intelGpu.AdapterRAM / 1MB)
            $result.DriverVersion = $intelGpu.DriverVersion
            return $result
        }
    } catch {}

    # -- Fallback: any GPU via WMI --
    try {
        $anyGpu = Get-CimInstance -ClassName Win32_VideoController | Sort-Object AdapterRAM -Descending | Select-Object -First 1
        if ($anyGpu) {
            $result.Model = $anyGpu.Name
            $result.VramMb = [math]::Round($anyGpu.AdapterRAM / 1MB)
            $result.DriverVersion = $anyGpu.DriverVersion
            $result.Backend = "vulkan"
        }
    } catch {}

    return $result
}

# -- Detect RAM --
function Get-RamMb {
    try {
        $os = Get-CimInstance -ClassName Win32_OperatingSystem
        return [math]::Round($os.TotalVisibleMemorySize / 1024)
    } catch {
        return 0
    }
}

# -- Detect Ollama --
function Get-OllamaInfo {
    $ollamaUrl = "http://localhost:11434"

    try {
        $version = Invoke-RestMethod -Uri "$ollamaUrl/api/version" -TimeoutSec 5
        $ollamaVersion = $version.version
    } catch {
        return $null
    }

    # Get loaded models
    try {
        $ps = Invoke-RestMethod -Uri "$ollamaUrl/api/ps" -TimeoutSec 5
        $modelsLoaded = @($ps.models | ForEach-Object { $_.name })
    } catch {
        $modelsLoaded = @()
    }

    # Get available models
    try {
        $tags = Invoke-RestMethod -Uri "$ollamaUrl/api/tags" -TimeoutSec 10
        $modelsAvailable = $tags.models.Count
    } catch {
        $modelsAvailable = 0
    }

    return @{
        OllamaVersion   = $ollamaVersion
        ModelsLoaded     = $modelsLoaded
        ModelsAvailable  = $modelsAvailable
    }
}

# -- IP Detection (Tailscale > LAN > localhost) --
function Get-BestIp {
    # Try Tailscale IP first (100.x.y.z)
    try {
        $tsIp = & tailscale ip -4 2>$null
        if ($LASTEXITCODE -eq 0 -and $tsIp) {
            return $tsIp.Trim()
        }
    } catch {}

    # Check for Tailscale network adapter
    try {
        $tsAdapter = Get-NetIPAddress -AddressFamily IPv4 | Where-Object {
            $_.IPAddress -match '^100\.'
        } | Select-Object -First 1
        if ($tsAdapter) {
            return $tsAdapter.IPAddress
        }
    } catch {}

    # LAN IP
    try {
        $adapters = Get-NetIPAddress -AddressFamily IPv4 | Where-Object {
            $_.IPAddress -ne '127.0.0.1' -and $_.PrefixOrigin -ne 'WellKnown'
        } | Sort-Object -Property InterfaceIndex
        if ($adapters) {
            return $adapters[0].IPAddress
        }
    } catch {}

    return "127.0.0.1"
}

# -- Calculate recommended Ollama config --
function Get-RecommendedConfig {
    param([int]$VramMb)

    $config = @{
        flash_attention = $true
        kv_cache_type   = "q8_0"
    }

    if ($VramMb -ge 24576) {
        $config.num_parallel    = 8
        $config.max_loaded_models = 4
        $config.max_queue       = 1024
        $config.keep_alive      = "30m"
        $config.context_length  = 16384
    } elseif ($VramMb -ge 12288) {
        $config.num_parallel    = 4
        $config.max_loaded_models = 2
        $config.max_queue       = 512
        $config.keep_alive      = "15m"
        $config.context_length  = 8192
    } elseif ($VramMb -ge 8192) {
        $config.num_parallel    = 2
        $config.max_loaded_models = 1
        $config.max_queue       = 256
        $config.keep_alive      = "10m"
        $config.context_length  = 4096
    } else {
        $config.num_parallel    = 1
        $config.max_loaded_models = 1
        $config.max_queue       = 128
        $config.keep_alive      = "5m"
        $config.context_length  = 2048
    }

    return $config
}

# -- Apply Ollama config --
function Set-OllamaConfig {
    param($Config)

    Write-Host "`n=== Applying Ollama Configuration ===" -ForegroundColor Cyan

    $envVars = @{
        "OLLAMA_NUM_PARALLEL"      = $Config.num_parallel
        "OLLAMA_MAX_LOADED_MODELS" = $Config.max_loaded_models
        "OLLAMA_MAX_QUEUE"         = $Config.max_queue
        "OLLAMA_KEEP_ALIVE"        = $Config.keep_alive
        "OLLAMA_FLASH_ATTENTION"   = if ($Config.flash_attention) { "1" } else { "0" }
        "OLLAMA_KV_CACHE_TYPE"     = $Config.kv_cache_type
        "OLLAMA_CONTEXT_LENGTH"    = $Config.context_length
    }

    foreach ($kv in $envVars.GetEnumerator()) {
        [System.Environment]::SetEnvironmentVariable($kv.Key, [string]$kv.Value, "Machine")
        Write-Host "  Set $($kv.Key) = $($kv.Value)" -ForegroundColor Green
    }

    # Restart Ollama service
    Write-Host "`nRestarting Ollama service..." -ForegroundColor Yellow
    try {
        Restart-Service -Name "OllamaService" -Force -ErrorAction Stop
        Start-Sleep -Seconds 3
        Write-Host "Ollama service restarted." -ForegroundColor Green
    } catch {
        Write-Warning "Could not restart Ollama service automatically. Please restart it manually."
    }
}

# ======================================================================
# MAIN
# ======================================================================

Write-Host @"

  _               _       _
 | |_  ___ _ _ __| |  ___| |_ _  _ _ _  ___
 | ' \/ -_) '_/ _`` | |___| _| || | ' \/ -_)
 |_||_\___|_| \__,_|     \__|\_,_|_||_\___|

  GPU Detection & Backend Configuration
  Version $HerdTuneVersion

"@ -ForegroundColor Cyan

# -- Hardware Detection --
Write-Host "=== Hardware Detection ===" -ForegroundColor Cyan

$gpu = Get-GpuDetailed
$ramMb = Get-RamMb

Write-Host "  GPU Vendor: $($gpu.Vendor)" -ForegroundColor $(if ($gpu.Vendor -ne 'none') { 'Green' } else { 'Yellow' })
Write-Host "  GPU Model:  $(if ($gpu.Model) { $gpu.Model } else { 'Not detected' })"
Write-Host "  VRAM:       $($gpu.VramMb) MB"
Write-Host "  RAM:        $ramMb MB"

if ($gpu.Vendor -eq 'nvidia') {
    Write-Host "  Driver:     $($gpu.DriverVersion)"
    Write-Host "  CUDA:       $($gpu.CudaVersion)"
    Write-Host "  Compute:    $($gpu.ComputeCap)"

    if ($gpu.IsBlackwell) {
        Write-Host ""
        Write-Host "  +==============================================================+" -ForegroundColor Yellow
        Write-Host "  |  BLACKWELL GPU DETECTED (compute capability $($gpu.ComputeCap))       |" -ForegroundColor Yellow
        Write-Host "  |                                                              |" -ForegroundColor Yellow
        Write-Host "  |  This GPU REQUIRES CUDA 13.x builds of llama-server.         |" -ForegroundColor Yellow
        Write-Host "  |  CUDA 12.x will silently fall back to CPU (~10x slower).     |" -ForegroundColor Yellow
        Write-Host "  |  herd-tune will select the correct cu13 binary.              |" -ForegroundColor Yellow
        Write-Host "  +==============================================================+" -ForegroundColor Yellow
        Write-Host ""
    } else {
        Write-Host "  Binary:     CUDA 12.x"
    }
} elseif ($gpu.Vendor -eq 'amd') {
    Write-Host "  Backend:    ROCm/HIP"
} elseif ($gpu.Vendor -eq 'intel') {
    Write-Host "  Backend:    SYCL"
} else {
    Write-Host "  Backend:    Vulkan (universal fallback)" -ForegroundColor Yellow
    $gpu.Backend = "vulkan"
}

# -- Backend Selection --
$resolvedBackend = $Backend
if ($Backend -eq "auto") {
    try {
        $null = Invoke-RestMethod -Uri "http://localhost:11434/api/version" -TimeoutSec 3
        $resolvedBackend = "ollama"
        Write-Host "`n  Auto-detected: Ollama is running on :11434" -ForegroundColor Cyan
    } catch {
        $resolvedBackend = "llama-server"
        Write-Host "`n  Auto-detected: Ollama not found, will set up llama-server" -ForegroundColor Cyan
    }
}

Write-Host "`n=== Backend: $resolvedBackend ===" -ForegroundColor Cyan

# -- Shared state --
$backendVersion = ""
$backendUrl = ""
$backendPort = 0
$configApplied = $false
$ollamaInfo = $null
$config = $null
$llamaBuildNumber = ""
$capabilities = @()
$modelPaths = @()
$modelsLoaded = @()
$maxContextLen = 2048

# ======================================================================
# OLLAMA BACKEND
# ======================================================================

if ($resolvedBackend -eq "ollama") {
    $ollamaInfo = Get-OllamaInfo
    if (-not $ollamaInfo) {
        Write-Error "Ollama is not running. Please start Ollama and try again."
        exit 1
    }

    Write-Host "  URL:      http://localhost:11434"
    Write-Host "  Version:  $($ollamaInfo.OllamaVersion)"
    Write-Host "  Models:   $($ollamaInfo.ModelsAvailable) available, $($ollamaInfo.ModelsLoaded.Count) loaded"

    $config = Get-RecommendedConfig -VramMb $gpu.VramMb

    Write-Host "`n=== Recommended Configuration ===" -ForegroundColor Cyan
    $config.GetEnumerator() | ForEach-Object { Write-Host "  $($_.Key): $($_.Value)" }

    if ($Apply) {
        Set-OllamaConfig -Config $config
        $configApplied = $true
    } else {
        Write-Host "`nRun with -Apply to set these environment variables and restart Ollama." -ForegroundColor Yellow
    }

    $backendVersion = $ollamaInfo.OllamaVersion
    $backendPort = 11434
    $modelsLoaded = $ollamaInfo.ModelsLoaded
    $maxContextLen = $config.context_length
}

# ======================================================================
# LLAMA-SERVER BACKEND
# ======================================================================

if ($resolvedBackend -eq "llama-server") {
    $herdDir = Join-Path $env:USERPROFILE ".herd"
    $binDir = Join-Path $herdDir "bin"
    if (-not (Test-Path $binDir)) {
        New-Item -ItemType Directory -Path $binDir -Force | Out-Null
    }

    Write-Host "`n=== llama-server Binary Download ===" -ForegroundColor Cyan

    # Determine correct asset pattern based on GPU vendor
    $assetPattern = ""
    switch ($gpu.Vendor) {
        "nvidia" {
            if ($gpu.CudaMajor -ge 13) {
                $assetPattern = "bin-win-cuda-cu13"
            } else {
                $assetPattern = "bin-win-cuda-cu12"
            }
        }
        "amd" {
            $assetPattern = "bin-win-hip"
        }
        "intel" {
            $assetPattern = "bin-win-sycl"
        }
        default {
            $assetPattern = "bin-win-vulkan"
        }
    }

    Write-Host "  GPU Vendor:     $($gpu.Vendor)"
    Write-Host "  Asset pattern:  $assetPattern"

    # Query latest release from llama.cpp GitHub
    Write-Host "  Querying llama.cpp releases..."
    try {
        $releaseData = Invoke-RestMethod -Uri "https://api.github.com/repos/ggml-org/llama.cpp/releases/latest" -TimeoutSec 30
    } catch {
        Write-Error "Could not fetch llama.cpp releases from GitHub API: $_"
        Write-Host "  Check your internet connection or try again later."
        Write-Host "  You can also manually download from: https://github.com/ggml-org/llama.cpp/releases"
        exit 1
    }

    $llamaTag = $releaseData.tag_name
    $llamaBuildNumber = $llamaTag
    Write-Host "  Latest release: $llamaTag"

    # Find matching asset
    $matchingAsset = $releaseData.assets | Where-Object {
        $_.name -like "*$assetPattern*" -and $_.name -like "*.zip"
    } | Select-Object -First 1

    if (-not $matchingAsset) {
        Write-Error "No matching binary found for pattern '$assetPattern'."
        Write-Host "  Available assets:" -ForegroundColor Yellow
        $releaseData.assets | ForEach-Object { Write-Host "    $($_.name)" }
        exit 1
    }

    $downloadUrl = $matchingAsset.browser_download_url
    $archiveName = $matchingAsset.name
    $archivePath = Join-Path $binDir $archiveName

    Write-Host "  Downloading:    $archiveName"
    Write-Host "  To:             $binDir"

    # Download with progress
    $ProgressPreference = 'Continue'
    try {
        Invoke-WebRequest -Uri $downloadUrl -OutFile $archivePath -UseBasicParsing
    } catch {
        Write-Error "Download failed: $_"
        exit 1
    }

    # Extract zip
    Write-Host "  Extracting..."
    try {
        Expand-Archive -Path $archivePath -DestinationPath $binDir -Force
        Remove-Item $archivePath -Force
    } catch {
        Write-Error "Extraction failed: $_"
        exit 1
    }

    # Find llama-server.exe
    $llamaServerBin = Get-ChildItem -Path $binDir -Recurse -Filter "llama-server.exe" | Select-Object -First 1
    if (-not $llamaServerBin) {
        Write-Error "llama-server.exe not found after extraction."
        Write-Host "  Contents of ${binDir}:" -ForegroundColor Yellow
        Get-ChildItem $binDir -Recurse | ForEach-Object { Write-Host "    $($_.FullName)" }
        exit 1
    }

    $llamaServerPath = $llamaServerBin.FullName
    Write-Host "  llama-server installed: $llamaServerPath" -ForegroundColor Green

    # -- VRAM-based context estimation --
    if ($Context -eq 4096 -and $gpu.VramMb -gt 0) {
        if ($gpu.VramMb -ge 24576) {
            $Context = 16384
        } elseif ($gpu.VramMb -ge 16384) {
            $Context = 8192
        } elseif ($gpu.VramMb -ge 8192) {
            $Context = 4096
        } else {
            $Context = 2048
        }
        Write-Host "  Context auto-set to $Context based on $($gpu.VramMb) MB VRAM"
    }

    $maxContextLen = $Context

    # -- Generate launch config --
    $confFile = Join-Path $herdDir "llama-server.conf"
    $modelFlag = ""
    if ($Model) {
        $modelFlag = "--model `"$Model`""
        $modelPaths = @($Model)
    }

    $timestamp = Get-Date -Format 'o'
    $confLines = @(
        "# llama-server launch configuration",
        "# Generated by herd-tune $HerdTuneVersion on $timestamp",
        "#",
        "# Start command:",
        "#   $llamaServerPath -ngl 99 -c $Context --port $Port $modelFlag",
        "#",
        "LLAMA_SERVER_BIN=$llamaServerPath",
        "LLAMA_SERVER_PORT=$Port",
        "LLAMA_SERVER_CTX=$Context",
        "LLAMA_SERVER_NGL=99",
        "GPU_VENDOR=$($gpu.Vendor)",
        "GPU_BACKEND=$($gpu.Backend)",
        "GPU_MODEL=$($gpu.Model)",
        "VRAM_MB=$($gpu.VramMb)",
        "MODEL_PATH=$Model",
        "BUILD=$llamaBuildNumber"
    )
    $confContent = $confLines -join "`n"
    Set-Content -Path $confFile -Value $confContent -Encoding UTF8
    Write-Host "  Launch config written: $confFile" -ForegroundColor Green

    Write-Host ""
    Write-Host "  To start llama-server:"
    Write-Host "    $llamaServerPath -ngl 99 -c $Context --port $Port $modelFlag"

    $backendVersion = $llamaBuildNumber
    $backendPort = $Port

    # Build capabilities
    switch ($gpu.Backend) {
        "cuda"   { $capabilities += "cuda" }
        "rocm"   { $capabilities += "rocm" }
        "sycl"   { $capabilities += "sycl" }
        "vulkan" { $capabilities += "vulkan" }
    }
    if ($gpu.Backend -in @("cuda", "rocm")) {
        $capabilities += "flash_attn"
    }
}

# ======================================================================
# IP DETECTION (Tailscale > LAN > localhost)
# ======================================================================

$bestIp = Get-BestIp

if ($resolvedBackend -eq "ollama") {
    $backendUrl = "http://${bestIp}:11434"
} else {
    $backendUrl = "http://${bestIp}:${Port}"
}

Write-Host "`n  Best reachable URL: $backendUrl" -ForegroundColor Cyan

# ======================================================================
# GENERATE STABLE MACHINE ID
# ======================================================================

$machineId = $null
try {
    $sid = (Get-CimInstance -ClassName Win32_UserAccount -Filter "LocalAccount=True" -ErrorAction SilentlyContinue |
        Select-Object -First 1).SID
    if ($sid) {
        $machineSid = $sid -replace '-\d+$'
        $sha = [System.Security.Cryptography.SHA256]::Create()
        $bytes = [System.Text.Encoding]::UTF8.GetBytes($machineSid)
        $hash = $sha.ComputeHash($bytes)
        $machineId = -join ($hash[0..15] | ForEach-Object { $_.ToString("x2") })
    }
} catch {}

# ======================================================================
# REGISTER WITH HERD
# ======================================================================

$regEndpoint = $null
if ($HerdEndpoint -and $HerdEndpoint -notmatch 'HERD_ENDPOINT') {
    $regEndpoint = $HerdEndpoint
}

if ($regEndpoint) {
    $regUrl = "$regEndpoint/api/nodes/register"
    if ($BakedEnrollmentKey -and $BakedEnrollmentKey -notmatch 'ENROLLMENT_KEY') {
        $regUrl += "?enrollment_key=$BakedEnrollmentKey"
    }

    Write-Host "`n=== Registering with Herd ===" -ForegroundColor Cyan
    Write-Host "  Endpoint: $regEndpoint"

    if ($resolvedBackend -eq "ollama") {
        $payloadObj = @{
            hostname           = $env:COMPUTERNAME.ToLower()
            backend            = "ollama"
            backend_version    = $ollamaInfo.OllamaVersion
            backend_url        = $backendUrl
            backend_port       = 11434
            gpu_vendor         = $gpu.Vendor
            gpu_model          = $gpu.Model
            gpu_backend        = $gpu.Backend
            gpu_driver_version = $gpu.DriverVersion
            cuda_version       = $gpu.CudaVersion
            vram_mb            = $gpu.VramMb
            ram_mb             = $ramMb
            ollama_version     = $ollamaInfo.OllamaVersion
            models_available   = $ollamaInfo.ModelsAvailable
            models_loaded      = $ollamaInfo.ModelsLoaded
            recommended_config = $config
            config_applied     = $configApplied
            max_context_len    = $maxContextLen
            capabilities       = $capabilities
            herd_tune_version  = $HerdTuneVersion
            os                 = "windows"
            registered_at      = (Get-Date -Format "o")
        }
    } else {
        $payloadObj = @{
            hostname           = $env:COMPUTERNAME.ToLower()
            backend            = "llama-server"
            backend_version    = $llamaBuildNumber
            backend_url        = $backendUrl
            backend_port       = $Port
            gpu_vendor         = $gpu.Vendor
            gpu_model          = $gpu.Model
            gpu_backend        = $gpu.Backend
            gpu_driver_version = $gpu.DriverVersion
            cuda_version       = $gpu.CudaVersion
            vram_mb            = $gpu.VramMb
            ram_mb             = $ramMb
            models_loaded      = @()
            model_paths        = $modelPaths
            capabilities       = $capabilities
            max_context_len    = $maxContextLen
            herd_tune_version  = $HerdTuneVersion
            os                 = "windows"
            registered_at      = (Get-Date -Format "o")
        }
    }

    if ($machineId) { $payloadObj.node_id = $machineId }
    $payload = $payloadObj | ConvertTo-Json -Depth 3

    try {
        $response = Invoke-RestMethod -Uri $regUrl `
            -Method Post -Body $payload -ContentType "application/json" -TimeoutSec 10
        Write-Host "  Status: $($response.status)" -ForegroundColor Green
        Write-Host "  $($response.message)" -ForegroundColor Green
    } catch {
        Write-Warning "Registration failed: $_"
        Write-Host "  You can register manually later by re-running this script with -Herd [url]"
    }
} else {
    Write-Host "`nNo Herd endpoint configured. Run with -Herd [url] to register." -ForegroundColor Yellow
}

# -- Daemon mode stub --
if ($Daemon) {
    Write-Host ""
    Write-Host "NOTE: Daemon mode (-Daemon) is not yet implemented." -ForegroundColor Yellow
    Write-Host "  Daemon mode will keep herd-tune resident with an HTTP control API for:"
    Write-Host "    POST /download-model  -- download a GGUF from HuggingFace"
    Write-Host "    POST /restart         -- restart llama-server"
    Write-Host "    GET  /status          -- current status"
    Write-Host '  Without daemon mode, dashboard control plane features (remote model download,'
    Write-Host '  llama-server restart) will not work for llama-server nodes.'
}

Write-Host ""
Write-Host 'Done!' -ForegroundColor Green
