#!/usr/bin/env bash
set -euo pipefail

# ── Herd Registration (auto-configured on download) ──
HERD_ENDPOINT="%%HERD_ENDPOINT%%"
ENROLLMENT_KEY="%%ENROLLMENT_KEY%%"
HERD_TUNE_VERSION="1.0.0"
APPLY=false
BACKEND="auto"
LLAMA_SERVER_PORT=8090
LLAMA_SERVER_CTX=4096
MODEL_PATH=""
DAEMON_MODE=false

# ── Colors ──
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

# ── Parse args ──
while [[ $# -gt 0 ]]; do
    case "$1" in
        --apply) APPLY=true; shift ;;
        --herd) HERD_ENDPOINT="$2"; shift 2 ;;
        --herd=*) HERD_ENDPOINT="${1#*=}"; shift ;;
        --enrollment-key) ENROLLMENT_KEY="$2"; shift 2 ;;
        --enrollment-key=*) ENROLLMENT_KEY="${1#*=}"; shift ;;
        --backend) BACKEND="$2"; shift 2 ;;
        --backend=*) BACKEND="${1#*=}"; shift ;;
        --port) LLAMA_SERVER_PORT="$2"; shift 2 ;;
        --port=*) LLAMA_SERVER_PORT="${1#*=}"; shift ;;
        --context) LLAMA_SERVER_CTX="$2"; shift 2 ;;
        --context=*) LLAMA_SERVER_CTX="${1#*=}"; shift ;;
        --model) MODEL_PATH="$2"; shift 2 ;;
        --model=*) MODEL_PATH="${1#*=}"; shift ;;
        --daemon) DAEMON_MODE=true; shift ;;
        -h|--help)
            echo "Usage: $0 [OPTIONS]"
            echo ""
            echo "Options:"
            echo "  --apply              Apply recommended OLLAMA_* env vars and restart Ollama"
            echo "  --herd URL           Herd endpoint URL for registration"
            echo "  --enrollment-key KEY Enrollment key for node registration"
            echo "  --backend TYPE       Backend type: ollama, llama-server, auto (default: auto)"
            echo "  --port PORT          llama-server port (default: 8090)"
            echo "  --context SIZE       llama-server context length (default: 4096)"
            echo "  --model PATH         Path to GGUF model file for llama-server"
            echo "  --daemon             Keep herd-tune resident with HTTP control API (stretch goal)"
            echo ""
            echo "Backend modes:"
            echo "  auto          If Ollama is running on :11434, use it; otherwise set up llama-server"
            echo "  ollama        Probe and configure existing Ollama installation"
            echo "  llama-server  Download and configure llama-server binary"
            exit 0
            ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

# Validate --backend
case "$BACKEND" in
    ollama|llama-server|auto) ;;
    *) echo "ERROR: Invalid --backend value '$BACKEND'. Must be: ollama, llama-server, auto"; exit 1 ;;
esac

echo ""
echo "  _               _       _"
echo " | |_  ___ _ _ __| |  ___| |_ _  _ _ _  ___"
echo " | ' \/ -_) '_/ _\` | |___| _| || | ' \/ -_)"
echo " |_||_\___|_| \__,_|     \__|\_,_|_||_\___|"
echo ""
echo "  GPU Detection & Backend Configuration"
echo -e "  Version ${CYAN}${HERD_TUNE_VERSION}${NC}"
echo ""

# ══════════════════════════════════════════════════════════════════════
# GPU VENDOR DETECTION
# ══════════════════════════════════════════════════════════════════════

GPU_VENDOR="none"
GPU_MODEL=""
GPU_BACKEND="cpu"
GPU_DRIVER_VERSION=""
CUDA_VERSION=""
COMPUTE_CAP=""
CUDA_MAJOR=0
VRAM_MB=0

detect_nvidia() {
    if ! command -v nvidia-smi &>/dev/null; then
        return 1
    fi

    # Query GPU name, VRAM, driver version
    local gpu_info
    gpu_info=$(nvidia-smi --query-gpu=name,memory.total,driver_version --format=csv,noheader,nounits 2>/dev/null || true)
    if [ -z "$gpu_info" ]; then
        return 1
    fi

    GPU_VENDOR="nvidia"
    GPU_MODEL=$(echo "$gpu_info" | head -1 | cut -d',' -f1 | xargs)
    VRAM_MB=$(echo "$gpu_info" | head -1 | cut -d',' -f2 | xargs)
    GPU_DRIVER_VERSION=$(echo "$gpu_info" | head -1 | cut -d',' -f3 | xargs)

    # Query compute capability
    COMPUTE_CAP=$(nvidia-smi --query-gpu=compute_cap --format=csv,noheader 2>/dev/null | head -1 | xargs || true)

    # Parse CUDA version from nvidia-smi header
    CUDA_VERSION=$(nvidia-smi 2>/dev/null | grep -oP 'CUDA Version: \K[0-9.]+' || true)

    # Determine CUDA major version needed based on compute capability
    # Blackwell (RTX 5000-series) has compute capability >= 12.0 and requires CUDA 13.x
    if [ -n "$COMPUTE_CAP" ]; then
        local cc_major
        cc_major=$(echo "$COMPUTE_CAP" | cut -d'.' -f1)
        if [ "$cc_major" -ge 12 ] 2>/dev/null; then
            CUDA_MAJOR=13
            GPU_BACKEND="cuda"
            echo ""
            echo -e "  ${BOLD}${YELLOW}╔══════════════════════════════════════════════════════════════╗${NC}"
            echo -e "  ${BOLD}${YELLOW}║  BLACKWELL GPU DETECTED (compute capability ${COMPUTE_CAP})       ║${NC}"
            echo -e "  ${BOLD}${YELLOW}║                                                              ║${NC}"
            echo -e "  ${BOLD}${YELLOW}║  This GPU REQUIRES CUDA 13.x builds of llama-server.         ║${NC}"
            echo -e "  ${BOLD}${YELLOW}║  CUDA 12.x will silently fall back to CPU (~10x slower).     ║${NC}"
            echo -e "  ${BOLD}${YELLOW}║  herd-tune will select the correct cu13 binary.              ║${NC}"
            echo -e "  ${BOLD}${YELLOW}╚══════════════════════════════════════════════════════════════╝${NC}"
            echo ""
        else
            CUDA_MAJOR=12
            GPU_BACKEND="cuda"
        fi
    else
        # Fallback: assume CUDA 12 if we can't read compute capability
        CUDA_MAJOR=12
        GPU_BACKEND="cuda"
    fi

    return 0
}

detect_amd() {
    # Try rocm-smi first
    if command -v rocm-smi &>/dev/null; then
        local rocm_info
        rocm_info=$(rocm-smi --showproductname --csv 2>/dev/null || true)
        if [ -n "$rocm_info" ]; then
            GPU_VENDOR="amd"
            GPU_BACKEND="rocm"
            # Parse product name from CSV output (skip header)
            GPU_MODEL=$(echo "$rocm_info" | tail -n +2 | head -1 | cut -d',' -f2 | xargs 2>/dev/null || echo "AMD GPU")

            # Get VRAM
            local vram_info
            vram_info=$(rocm-smi --showmeminfo vram --csv 2>/dev/null || true)
            if [ -n "$vram_info" ]; then
                # Total VRAM is in bytes, convert to MB
                local vram_bytes
                vram_bytes=$(echo "$vram_info" | tail -n +2 | head -1 | grep -oP '\d+' | head -1 || echo "0")
                VRAM_MB=$(( vram_bytes / 1048576 ))
            fi

            # Get GPU arch via rocminfo
            if command -v rocminfo &>/dev/null; then
                local arch
                arch=$(rocminfo 2>/dev/null | grep -oP 'gfx\d+' | head -1 || true)
                if [ -n "$arch" ]; then
                    GPU_MODEL="${GPU_MODEL} (${arch})"
                fi
            fi

            return 0
        fi
    fi

    # Try hipconfig
    if command -v hipconfig &>/dev/null; then
        GPU_VENDOR="amd"
        GPU_BACKEND="rocm"
        GPU_MODEL=$(hipconfig --platform 2>/dev/null || echo "AMD GPU")
        return 0
    fi

    # Check lspci for AMD GPUs
    if command -v lspci &>/dev/null; then
        local amd_gpu
        amd_gpu=$(lspci 2>/dev/null | grep -i 'VGA\|3D' | grep -i 'AMD\|Radeon' | head -1 || true)
        if [ -n "$amd_gpu" ]; then
            GPU_VENDOR="amd"
            GPU_BACKEND="rocm"
            GPU_MODEL=$(echo "$amd_gpu" | sed 's/.*: //')
            return 0
        fi
    fi

    return 1
}

detect_intel() {
    # Try sycl-ls
    if command -v sycl-ls &>/dev/null; then
        local sycl_info
        sycl_info=$(sycl-ls 2>/dev/null || true)
        if echo "$sycl_info" | grep -qi 'intel\|level_zero'; then
            GPU_VENDOR="intel"
            GPU_BACKEND="sycl"
            GPU_MODEL=$(echo "$sycl_info" | grep -i 'intel' | head -1 | xargs || echo "Intel GPU")
            return 0
        fi
    fi

    # Check lspci for Intel discrete GPUs (Arc)
    if command -v lspci &>/dev/null; then
        local intel_gpu
        intel_gpu=$(lspci 2>/dev/null | grep -i 'VGA\|3D' | grep -i 'Intel.*Arc\|Intel.*Xe' | head -1 || true)
        if [ -n "$intel_gpu" ]; then
            GPU_VENDOR="intel"
            GPU_BACKEND="sycl"
            GPU_MODEL=$(echo "$intel_gpu" | sed 's/.*: //')
            return 0
        fi
    fi

    return 1
}

echo -e "${CYAN}=== Hardware Detection ===${NC}"

# Try GPU detection in order: NVIDIA > AMD > Intel > fallback
if detect_nvidia; then
    echo -e "  GPU Vendor: ${GREEN}NVIDIA${NC}"
    echo "  GPU Model:  $GPU_MODEL"
    echo "  VRAM:       ${VRAM_MB} MB"
    echo "  Driver:     $GPU_DRIVER_VERSION"
    echo "  CUDA:       $CUDA_VERSION"
    echo "  Compute:    $COMPUTE_CAP"
    if [ "$CUDA_MAJOR" -eq 13 ]; then
        echo -e "  Binary:     ${YELLOW}CUDA 13.x required (Blackwell)${NC}"
    else
        echo "  Binary:     CUDA 12.x"
    fi
elif detect_amd; then
    echo -e "  GPU Vendor: ${GREEN}AMD${NC}"
    echo "  GPU Model:  $GPU_MODEL"
    echo "  VRAM:       ${VRAM_MB} MB"
    echo "  Backend:    ROCm"
elif detect_intel; then
    echo -e "  GPU Vendor: ${GREEN}Intel${NC}"
    echo "  GPU Model:  $GPU_MODEL"
    echo "  Backend:    SYCL"
else
    echo -e "  GPU Vendor: ${YELLOW}None detected${NC}"
    echo "  Backend:    Vulkan (universal fallback)"
    GPU_BACKEND="vulkan"
fi

# Detect RAM
RAM_MB=$(free -m 2>/dev/null | awk '/^Mem:/{print $2}' || echo 0)
echo "  RAM:        ${RAM_MB} MB"

# ══════════════════════════════════════════════════════════════════════
# BACKEND SELECTION
# ══════════════════════════════════════════════════════════════════════

RESOLVED_BACKEND="$BACKEND"
if [ "$BACKEND" = "auto" ]; then
    # Check if Ollama is running on port 11434
    if curl -sf "http://localhost:11434/api/version" --connect-timeout 3 &>/dev/null; then
        RESOLVED_BACKEND="ollama"
        echo ""
        echo -e "  ${CYAN}Auto-detected: Ollama is running on :11434${NC}"
    else
        RESOLVED_BACKEND="llama-server"
        echo ""
        echo -e "  ${CYAN}Auto-detected: Ollama not found, will set up llama-server${NC}"
    fi
fi

echo ""
echo -e "${CYAN}=== Backend: ${BOLD}${RESOLVED_BACKEND}${NC} ==="

# ══════════════════════════════════════════════════════════════════════
# OLLAMA BACKEND
# ══════════════════════════════════════════════════════════════════════

OLLAMA_VERSION=""
OLLAMA_URL=""
MODELS_LOADED="[]"
MODELS_AVAILABLE=0
BACKEND_VERSION=""
BACKEND_URL=""
BACKEND_PORT=0
CONFIG_APPLIED=false

# Recommended config vars (used by both paths)
NUM_PARALLEL=1
MAX_LOADED=1
MAX_QUEUE=128
KEEP_ALIVE="5m"
CTX_LEN=2048

if [ "$RESOLVED_BACKEND" = "ollama" ]; then
    OLLAMA_URL="http://localhost:11434"

    OLLAMA_VERSION=$(curl -sf "${OLLAMA_URL}/api/version" 2>/dev/null | grep -o '"version":"[^"]*"' | cut -d'"' -f4 || true)
    if [ -z "$OLLAMA_VERSION" ]; then
        echo -e "${RED}ERROR: Ollama is not running at ${OLLAMA_URL}${NC}"
        exit 1
    fi

    # Get loaded models
    MODELS_LOADED=$(curl -sf "${OLLAMA_URL}/api/ps" 2>/dev/null | python3 -c "
import sys, json
try:
    data = json.load(sys.stdin)
    names = [m['name'] for m in data.get('models', [])]
    print(json.dumps(names))
except: print('[]')
" 2>/dev/null || echo '[]')

    # Get available model count
    MODELS_AVAILABLE=$(curl -sf "${OLLAMA_URL}/api/tags" 2>/dev/null | python3 -c "
import sys, json
try:
    data = json.load(sys.stdin)
    print(len(data.get('models', [])))
except: print(0)
" 2>/dev/null || echo 0)

    echo "  URL:      ${OLLAMA_URL}"
    echo "  Version:  ${OLLAMA_VERSION}"
    echo "  Models:   ${MODELS_AVAILABLE} available"

    # Calculate recommended config based on VRAM
    if [ "$VRAM_MB" -ge 24576 ]; then
        NUM_PARALLEL=8; MAX_LOADED=4; MAX_QUEUE=1024; KEEP_ALIVE="30m"; CTX_LEN=16384
    elif [ "$VRAM_MB" -ge 12288 ]; then
        NUM_PARALLEL=4; MAX_LOADED=2; MAX_QUEUE=512; KEEP_ALIVE="15m"; CTX_LEN=8192
    elif [ "$VRAM_MB" -ge 8192 ]; then
        NUM_PARALLEL=2; MAX_LOADED=1; MAX_QUEUE=256; KEEP_ALIVE="10m"; CTX_LEN=4096
    fi

    echo ""
    echo -e "${CYAN}=== Recommended Configuration ===${NC}"
    echo "  num_parallel:      $NUM_PARALLEL"
    echo "  max_loaded_models: $MAX_LOADED"
    echo "  max_queue:         $MAX_QUEUE"
    echo "  keep_alive:        $KEEP_ALIVE"
    echo "  flash_attention:   true"
    echo "  kv_cache_type:     q8_0"
    echo "  context_length:    $CTX_LEN"

    # Apply config
    if [ "$APPLY" = true ]; then
        echo ""
        echo -e "${CYAN}=== Applying Ollama Configuration ===${NC}"

        OLLAMA_ENV_FILE="/etc/systemd/system/ollama.service.d/override.conf"
        mkdir -p "$(dirname "$OLLAMA_ENV_FILE")" 2>/dev/null || {
            echo -e "${YELLOW}WARNING: Cannot create systemd override directory (not root?). Skipping apply.${NC}"
            APPLY=false
        }

        if [ "$APPLY" = true ]; then
            cat > "$OLLAMA_ENV_FILE" << ENVEOF
[Service]
Environment="OLLAMA_NUM_PARALLEL=${NUM_PARALLEL}"
Environment="OLLAMA_MAX_LOADED_MODELS=${MAX_LOADED}"
Environment="OLLAMA_MAX_QUEUE=${MAX_QUEUE}"
Environment="OLLAMA_KEEP_ALIVE=${KEEP_ALIVE}"
Environment="OLLAMA_FLASH_ATTENTION=1"
Environment="OLLAMA_KV_CACHE_TYPE=q8_0"
Environment="OLLAMA_CONTEXT_LENGTH=${CTX_LEN}"
ENVEOF

            echo "  Wrote ${OLLAMA_ENV_FILE}"

            echo "  Restarting Ollama service..."
            systemctl daemon-reload
            systemctl restart ollama
            sleep 3
            echo -e "  ${GREEN}Ollama service restarted.${NC}"
            CONFIG_APPLIED=true
        fi
    else
        echo ""
        echo "Run with --apply to set these environment variables and restart Ollama."
    fi

    BACKEND_VERSION="$OLLAMA_VERSION"
    BACKEND_PORT=11434

    # Determine best reachable URL
    BEST_URL="$OLLAMA_URL"
fi

# ══════════════════════════════════════════════════════════════════════
# LLAMA-SERVER BACKEND
# ══════════════════════════════════════════════════════════════════════

LLAMA_BUILD_NUMBER=""
CAPABILITIES="[]"
MODEL_PATHS="[]"

if [ "$RESOLVED_BACKEND" = "llama-server" ]; then
    HERD_DIR="$HOME/.herd"
    BIN_DIR="${HERD_DIR}/bin"
    mkdir -p "$BIN_DIR"

    echo ""
    echo -e "${CYAN}=== llama-server Binary Download ===${NC}"

    # Determine correct asset pattern based on GPU vendor
    ASSET_PATTERN=""
    case "$GPU_VENDOR" in
        nvidia)
            if [ "$CUDA_MAJOR" -ge 13 ]; then
                ASSET_PATTERN="bin-ubuntu-x64-cuda-cu13"
            else
                ASSET_PATTERN="bin-ubuntu-x64-cuda-cu12"
            fi
            ;;
        amd)
            ASSET_PATTERN="bin-ubuntu-x64-rocm"
            ;;
        *)
            # Intel SYCL has no reliable pre-built Linux binary; use Vulkan as fallback
            ASSET_PATTERN="bin-ubuntu-x64-vulkan" # Vulkan fallback if available, else CPU
            if [ "$GPU_VENDOR" = "intel" ]; then
                echo -e "  ${YELLOW}NOTE: No pre-built SYCL Linux binaries available.${NC}"
                echo -e "  ${YELLOW}Using Vulkan fallback. For best Intel GPU performance, build from source with -DGGML_SYCL=ON${NC}"
            fi
            ;;
    esac

    echo "  GPU Vendor:     $GPU_VENDOR"
    echo "  Asset pattern:  $ASSET_PATTERN"

    # Query latest release from llama.cpp GitHub
    echo "  Querying llama.cpp releases..."
    RELEASE_JSON=$(curl -sf "https://api.github.com/repos/ggml-org/llama.cpp/releases/latest" 2>/dev/null || true)

    if [ -z "$RELEASE_JSON" ]; then
        echo -e "${RED}ERROR: Could not fetch llama.cpp releases from GitHub API.${NC}"
        echo "  Check your internet connection or try again later."
        echo "  You can also manually download from: https://github.com/ggml-org/llama.cpp/releases"
        exit 1
    fi

    # Extract tag name (build number)
    LLAMA_TAG=$(echo "$RELEASE_JSON" | python3 -c "
import sys, json
try:
    data = json.load(sys.stdin)
    print(data.get('tag_name', ''))
except: pass
" 2>/dev/null || true)

    LLAMA_BUILD_NUMBER="$LLAMA_TAG"
    echo "  Latest release: $LLAMA_TAG"

    # Find matching asset URL
    DOWNLOAD_URL=$(echo "$RELEASE_JSON" | python3 -c "
import sys, json
pattern = '$ASSET_PATTERN'
try:
    data = json.load(sys.stdin)
    for asset in data.get('assets', []):
        name = asset['name']
        if pattern in name and name.endswith('.tar.gz'):
            print(asset['browser_download_url'])
            break
except: pass
" 2>/dev/null || true)

    if [ -z "$DOWNLOAD_URL" ]; then
        echo -e "${RED}ERROR: No matching binary found for pattern '${ASSET_PATTERN}'.${NC}"
        echo "  Available assets can be checked at: https://github.com/ggml-org/llama.cpp/releases/latest"
        exit 1
    fi

    ARCHIVE_NAME=$(basename "$DOWNLOAD_URL")
    ARCHIVE_PATH="${BIN_DIR}/${ARCHIVE_NAME}"

    echo "  Downloading:    $ARCHIVE_NAME"
    echo "  To:             $BIN_DIR"

    # Download with progress
    if command -v wget &>/dev/null; then
        wget --progress=bar:force -O "$ARCHIVE_PATH" "$DOWNLOAD_URL" 2>&1
    else
        curl -L --progress-bar -o "$ARCHIVE_PATH" "$DOWNLOAD_URL"
    fi

    echo "  Extracting..."
    tar -xzf "$ARCHIVE_PATH" -C "$BIN_DIR"
    rm -f "$ARCHIVE_PATH"

    # Find the llama-server binary in extracted files
    LLAMA_SERVER_BIN=$(find "$BIN_DIR" -name 'llama-server' -type f 2>/dev/null | head -1 || true)
    if [ -z "$LLAMA_SERVER_BIN" ]; then
        # May be in a subdirectory
        LLAMA_SERVER_BIN=$(find "$BIN_DIR" -name 'llama-server' 2>/dev/null | head -1 || true)
    fi

    if [ -z "$LLAMA_SERVER_BIN" ]; then
        echo -e "${RED}ERROR: llama-server binary not found after extraction.${NC}"
        echo "  Contents of $BIN_DIR:"
        ls -la "$BIN_DIR" 2>/dev/null || true
        exit 1
    fi

    chmod +x "$LLAMA_SERVER_BIN"
    echo -e "  ${GREEN}llama-server installed: $LLAMA_SERVER_BIN${NC}"

    # ── VRAM-based context estimation ──
    # Each 1K context ~ 0.5GB VRAM for a 7B model. Be conservative.
    if [ "$LLAMA_SERVER_CTX" -eq 4096 ] && [ "$VRAM_MB" -gt 0 ]; then
        if [ "$VRAM_MB" -ge 24576 ]; then
            LLAMA_SERVER_CTX=16384
        elif [ "$VRAM_MB" -ge 16384 ]; then
            LLAMA_SERVER_CTX=8192
        elif [ "$VRAM_MB" -ge 8192 ]; then
            LLAMA_SERVER_CTX=4096
        else
            LLAMA_SERVER_CTX=2048
        fi
        echo "  Context auto-set to ${LLAMA_SERVER_CTX} based on ${VRAM_MB} MB VRAM"
    fi

    CTX_LEN=$LLAMA_SERVER_CTX

    # ── Generate launch config ──
    CONF_FILE="${HERD_DIR}/llama-server.conf"
    MODEL_FLAG=""
    if [ -n "$MODEL_PATH" ]; then
        MODEL_FLAG="--model ${MODEL_PATH}"
        MODEL_PATHS="[\"${MODEL_PATH}\"]"
    fi

    cat > "$CONF_FILE" << CONFEOF
# llama-server launch configuration
# Generated by herd-tune $HERD_TUNE_VERSION on $(date -u +"%Y-%m-%dT%H:%M:%SZ")
#
# Start command:
#   $LLAMA_SERVER_BIN -ngl 99 -c $LLAMA_SERVER_CTX --port $LLAMA_SERVER_PORT $MODEL_FLAG
#
LLAMA_SERVER_BIN=$LLAMA_SERVER_BIN
LLAMA_SERVER_PORT=$LLAMA_SERVER_PORT
LLAMA_SERVER_CTX=$LLAMA_SERVER_CTX
LLAMA_SERVER_NGL=99
GPU_VENDOR=$GPU_VENDOR
GPU_BACKEND=$GPU_BACKEND
GPU_MODEL=$GPU_MODEL
VRAM_MB=$VRAM_MB
MODEL_PATH=$MODEL_PATH
BUILD=$LLAMA_BUILD_NUMBER
CONFEOF

    echo -e "  ${GREEN}Launch config written: $CONF_FILE${NC}"
    echo ""
    echo "  To start llama-server:"
    echo "    $LLAMA_SERVER_BIN -ngl 99 -c $LLAMA_SERVER_CTX --port $LLAMA_SERVER_PORT $MODEL_FLAG"

    BACKEND_VERSION="$LLAMA_BUILD_NUMBER"
    BACKEND_PORT=$LLAMA_SERVER_PORT

    # Build capabilities list
    CAPS=()
    case "$GPU_BACKEND" in
        cuda)  CAPS+=("cuda") ;;
        rocm)  CAPS+=("rocm") ;;
        sycl)  CAPS+=("sycl") ;;
        vulkan) CAPS+=("vulkan") ;;
    esac
    # llama-server supports flash attention by default on CUDA
    if [ "$GPU_BACKEND" = "cuda" ] || [ "$GPU_BACKEND" = "rocm" ]; then
        CAPS+=("flash_attn")
    fi
    CAPABILITIES=$(printf '%s\n' "${CAPS[@]}" | python3 -c "
import sys, json
print(json.dumps([l.strip() for l in sys.stdin if l.strip()]))
" 2>/dev/null || echo '[]')
fi

# ══════════════════════════════════════════════════════════════════════
# IP DETECTION (Tailscale > LAN > localhost)
# ══════════════════════════════════════════════════════════════════════

detect_best_ip() {
    # Prefer Tailscale IP (100.x.y.z range)
    if command -v tailscale &>/dev/null; then
        local ts_ip
        ts_ip=$(tailscale ip -4 2>/dev/null || true)
        if [ -n "$ts_ip" ]; then
            echo "$ts_ip"
            return
        fi
    fi

    # Try to find Tailscale IP from interface
    local ts_iface_ip
    ts_iface_ip=$(ip addr show 2>/dev/null | grep -oP '100\.\d+\.\d+\.\d+' | head -1 || true)
    if [ -n "$ts_iface_ip" ]; then
        echo "$ts_iface_ip"
        return
    fi

    # LAN IP
    local lan_ip
    lan_ip=$(hostname -I 2>/dev/null | awk '{print $1}' || true)
    if [ -n "$lan_ip" ]; then
        echo "$lan_ip"
        return
    fi

    echo "127.0.0.1"
}

BEST_IP=$(detect_best_ip)

if [ "$RESOLVED_BACKEND" = "ollama" ]; then
    BACKEND_URL="http://${BEST_IP}:11434"
else
    BACKEND_URL="http://${BEST_IP}:${LLAMA_SERVER_PORT}"
fi

echo ""
echo -e "  ${CYAN}Best reachable URL: ${BACKEND_URL}${NC}"

# ══════════════════════════════════════════════════════════════════════
# GENERATE STABLE MACHINE ID
# ══════════════════════════════════════════════════════════════════════

NODE_ID=""
if [ -f /etc/machine-id ]; then
    NODE_ID=$(cat /etc/machine-id)
elif [ -f /var/lib/dbus/machine-id ]; then
    NODE_ID=$(cat /var/lib/dbus/machine-id)
else
    MAC=$(ip link show 2>/dev/null | awk '/ether/{print $2; exit}' || true)
    NODE_ID=$(echo -n "${MAC}$(hostname)" | sha256sum | cut -d' ' -f1 | head -c 32)
fi

# ══════════════════════════════════════════════════════════════════════
# REGISTER WITH HERD
# ══════════════════════════════════════════════════════════════════════

if [ -n "$HERD_ENDPOINT" ] && [ "$HERD_ENDPOINT" != '%%HERD_ENDPOINT%%' ]; then
    echo ""
    echo -e "${CYAN}=== Registering with Herd ===${NC}"
    echo "  Endpoint: ${HERD_ENDPOINT}"

    REG_URL="${HERD_ENDPOINT}/api/nodes/register"
    if [ -n "$ENROLLMENT_KEY" ] && [ "$ENROLLMENT_KEY" != '%%ENROLLMENT_KEY%%' ]; then
        REG_URL="${REG_URL}?enrollment_key=${ENROLLMENT_KEY}"
    fi

    HOSTNAME_VAL=$(hostname | tr '[:upper:]' '[:lower:]')
    REGISTERED_AT=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
    NODE_ID_JSON=""
    if [ -n "$NODE_ID" ]; then
        NODE_ID_JSON="\"node_id\": \"${NODE_ID}\","
    fi

    # Build the appropriate JSON payload based on backend type
    if [ "$RESOLVED_BACKEND" = "ollama" ]; then
        PAYLOAD=$(cat << JSONEOF
{
  ${NODE_ID_JSON}
  "hostname": "${HOSTNAME_VAL}",
  "backend": "ollama",
  "backend_version": "${OLLAMA_VERSION}",
  "backend_url": "${BACKEND_URL}",
  "backend_port": 11434,
  "gpu_vendor": "${GPU_VENDOR}",
  "gpu_model": "${GPU_MODEL}",
  "gpu_backend": "${GPU_BACKEND}",
  "gpu_driver_version": "${GPU_DRIVER_VERSION}",
  "cuda_version": "${CUDA_VERSION}",
  "vram_mb": ${VRAM_MB},
  "ram_mb": ${RAM_MB},
  "ollama_version": "${OLLAMA_VERSION}",
  "models_available": ${MODELS_AVAILABLE},
  "models_loaded": ${MODELS_LOADED},
  "recommended_config": {
    "num_parallel": ${NUM_PARALLEL},
    "max_loaded_models": ${MAX_LOADED},
    "max_queue": ${MAX_QUEUE},
    "keep_alive": "${KEEP_ALIVE}",
    "flash_attention": true,
    "kv_cache_type": "q8_0",
    "context_length": ${CTX_LEN}
  },
  "config_applied": ${CONFIG_APPLIED},
  "max_context_len": ${CTX_LEN},
  "capabilities": ${CAPABILITIES:-"[]"},
  "herd_tune_version": "${HERD_TUNE_VERSION}",
  "os": "linux",
  "registered_at": "${REGISTERED_AT}"
}
JSONEOF
)
    else
        # llama-server payload
        PAYLOAD=$(cat << JSONEOF
{
  ${NODE_ID_JSON}
  "hostname": "${HOSTNAME_VAL}",
  "backend": "llama-server",
  "backend_version": "${LLAMA_BUILD_NUMBER}",
  "backend_url": "${BACKEND_URL}",
  "backend_port": ${LLAMA_SERVER_PORT},
  "gpu_vendor": "${GPU_VENDOR}",
  "gpu_model": "${GPU_MODEL}",
  "gpu_backend": "${GPU_BACKEND}",
  "gpu_driver_version": "${GPU_DRIVER_VERSION}",
  "cuda_version": "${CUDA_VERSION}",
  "vram_mb": ${VRAM_MB},
  "ram_mb": ${RAM_MB},
  "models_loaded": [],
  "model_paths": ${MODEL_PATHS},
  "capabilities": ${CAPABILITIES},
  "max_context_len": ${LLAMA_SERVER_CTX},
  "herd_tune_version": "${HERD_TUNE_VERSION}",
  "os": "linux",
  "registered_at": "${REGISTERED_AT}"
}
JSONEOF
)
    fi

    RESPONSE=$(curl -sf -X POST "${REG_URL}" \
        -H "Content-Type: application/json" \
        -d "$PAYLOAD" 2>/dev/null || true)

    if [ -n "$RESPONSE" ]; then
        echo -e "  ${GREEN}Registration successful!${NC}"
        echo "  $RESPONSE"
    else
        echo -e "  ${YELLOW}WARNING: Registration failed. You can register later with --herd <url>${NC}"
    fi
else
    echo ""
    echo "No Herd endpoint configured. Run with --herd <url> to register."
fi

# ── Daemon mode stub ──
if [ "$DAEMON_MODE" = true ]; then
    echo ""
    echo -e "${YELLOW}NOTE: Daemon mode (--daemon) is not yet implemented.${NC}"
    echo "  Daemon mode will keep herd-tune resident with an HTTP control API for:"
    echo "    POST /download-model  — download a GGUF from HuggingFace"
    echo "    POST /restart         — restart llama-server"
    echo "    GET  /status          — current status"
    echo "  Without daemon mode, dashboard control plane features (remote model download,"
    echo "  llama-server restart) will not work for llama-server nodes."
fi

echo ""
echo -e "${GREEN}Done!${NC}"
