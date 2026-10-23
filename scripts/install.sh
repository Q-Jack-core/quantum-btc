#!/usr/bin/env bash
# =============================================================================
# Q-BTC Core: Linux installer (Debian / Ubuntu)
#
#   curl -fsSL https://raw.githubusercontent.com/Q-Jack-core/quantum-btc/main/scripts/install.sh | bash
#   curl -fsSL <same url> | bash -s -- --tag v2.6.0
#
# What it does
#   1. Checks the machine (OS, CPU arch, RAM, disk, NVIDIA GPU, CUDA toolkit).
#   2. Installs build dependencies with apt and Rust with rustup.
#   3. Clones the repository and checks out a release tag (default: newest).
#   4. Builds with `cargo build --release`. build.rs compiles the CUDA kernel
#      when a toolkit is found and falls back to a CPU stub otherwise.
#   5. Tells you plainly whether the binary can mine on the GPU.
#   6. Installs a small launcher, `qbtc`, into ~/.local/bin (and makes it usable
#      right away: symlink in /usr/local/bin when run as root, PATH line in ~/.bashrc).
#   7. Downloads the live dashboard (qbtc-monitor.py) for `qbtc monitor`.
#
# It never asks questions (it must work under `curl | bash`), never touches an
# existing data directory, and never creates or handles wallets or seed phrases.
# Run with --check to see what it would do without changing anything.
# =============================================================================
set -euo pipefail

REPO_URL="${QBTC_REPO:-https://github.com/Q-Jack-core/quantum-btc.git}"
TAG="${QBTC_TAG:-}"
DIR="${QBTC_DIR:-$HOME/quantum-btc}"
DATADIR="${QBTC_DATADIR:-$HOME/.qbtc}"
PORT="${QBTC_PORT:-8000}"
INSTALL_CUDA=0
CHECK_ONLY=0
MONITOR_URL="${QBTC_MONITOR_URL:-https://raw.githubusercontent.com/Q-Jack-core/quantum-btc/main/scripts/qbtc-monitor.py}"
MONITOR_FILE="${QBTC_MONITOR_FILE:-$HOME/.local/share/qbtc/qbtc-monitor.py}"

SUDO=""
NV_GPUS=""
CUDA_HOME_DIR=""
CUDA_LIB_DIR=""
GPU_BUILD="unknown"

PKGS=(build-essential clang libclang-dev cmake pkg-config libssl-dev git tmux curl ca-certificates)

say()  { printf '\n==> %s\n' "$*"; }
info() { printf '    %s\n' "$*"; }
warn() { printf 'WARN: %s\n' "$*" >&2; }
die()  { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

usage() {
  cat <<'USAGE'
Q-BTC Linux installer

Usage: install.sh [options]

  --tag <vX.Y.Z>    Release tag to build (default: newest vX.Y.Z tag)
  --dir <path>      Source directory         (default: ~/quantum-btc)
  --datadir <path>  Node data/wallet folder  (default: ~/.qbtc)
  --port <port>     P2P port                 (default: 8000)
  --install-cuda    If an NVIDIA GPU is present but nvcc is missing, install the
                    distribution package nvidia-cuda-toolkit first
  --check           Only report what was detected; change nothing
  -h, --help        Show this help

Environment overrides: QBTC_TAG, QBTC_DIR, QBTC_DATADIR, QBTC_PORT, QBTC_REPO,
CUDA_PATH (path to a CUDA toolkit, same meaning as in build.rs) and
QBTC_MONITOR_URL (where the dashboard is downloaded from).
USAGE
}

parse_args() {
  while [ $# -gt 0 ]; do
    case "$1" in
      --tag)          [ $# -ge 2 ] || die "--tag needs a value";     TAG="$2";     shift 2 ;;
      --dir)          [ $# -ge 2 ] || die "--dir needs a value";     DIR="$2";     shift 2 ;;
      --datadir)      [ $# -ge 2 ] || die "--datadir needs a value"; DATADIR="$2"; shift 2 ;;
      --port)         [ $# -ge 2 ] || die "--port needs a value";    PORT="$2";    shift 2 ;;
      --install-cuda) INSTALL_CUDA=1; shift ;;
      --check)        CHECK_ONLY=1;   shift ;;
      -h|--help)      usage; exit 0 ;;
      *)              die "Unknown option: $1 (try --help)" ;;
    esac
  done

  if [ -n "$TAG" ] && ! [[ "$TAG" =~ ^[A-Za-z0-9._-]+$ ]]; then
    die "Invalid tag: $TAG"
  fi
  if ! [[ "$PORT" =~ ^[0-9]+$ ]] || [ "$PORT" -lt 1 ] || [ "$PORT" -gt 65534 ]; then
    die "Invalid port: $PORT"
  fi
}

preflight() {
  [ "$(uname -s)" = "Linux" ] || die "This installer is for Linux. On Windows/macOS use the Desktop GUI or the Start_Mining launchers (see README)."

  case "$(uname -m)" in
    x86_64) ;;
    aarch64|arm64) warn "ARM64 is untested. CPU mining may work; GPU mining is not supported by this installer." ;;
    *) die "Unsupported CPU architecture: $(uname -m)" ;;
  esac

  if ! command -v apt-get >/dev/null 2>&1; then
    die "apt-get not found. This installer supports Debian/Ubuntu. On other distributions install: a C/C++ toolchain, clang, libclang, cmake, pkg-config, OpenSSL headers, git, tmux and Rust, then run 'cargo build --release' (see docs/LINUX.md)."
  fi

  local mem_kb disk_kb
  mem_kb="$(awk '/^MemTotal:/ {print $2}' /proc/meminfo 2>/dev/null || true)"
  mem_kb="${mem_kb:-0}"
  if [ "$mem_kb" -lt 4000000 ]; then
    warn "Less than 4 GB RAM detected. The release build uses fat LTO and may run out of memory; add swap if it fails."
  fi
  disk_kb="$(df -Pk "$HOME" 2>/dev/null | awk 'NR==2 {print $4}' || true)"
  disk_kb="${disk_kb:-0}"
  if [ "$disk_kb" -lt 8000000 ]; then
    warn "Less than 8 GB free in $HOME. The build needs several GB of disk."
  fi
}

detect_gpu() {
  NV_GPUS=""
  if command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi -L >/dev/null 2>&1; then
    NV_GPUS="$(nvidia-smi --query-gpu=name,compute_cap --format=csv,noheader 2>/dev/null \
      || nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null || true)"
  fi
}

# Mirrors the search order in build.rs so we report what the build will see.
find_cuda() {
  local c
  if [ -n "${CUDA_PATH:-}" ] && [ -x "$CUDA_PATH/bin/nvcc" ]; then
    echo "$CUDA_PATH"; return 0
  fi
  for c in /usr/local/cuda /opt/cuda /usr; do
    if [ -x "$c/bin/nvcc" ]; then echo "$c"; return 0; fi
  done
  c="$(ls -d /usr/local/cuda-* 2>/dev/null | sort -V | tail -1 || true)"
  if [ -n "$c" ] && [ -x "$c/bin/nvcc" ]; then echo "$c"; return 0; fi
  return 1
}

detect_cuda() {
  local sub
  CUDA_HOME_DIR="$(find_cuda || true)"
  CUDA_LIB_DIR=""
  # /usr (distro toolkit) keeps its libraries on the default linker path.
  if [ -n "$CUDA_HOME_DIR" ] && [ "$CUDA_HOME_DIR" != "/usr" ]; then
    for sub in lib64 lib; do
      if [ -d "$CUDA_HOME_DIR/$sub" ]; then CUDA_LIB_DIR="$CUDA_HOME_DIR/$sub"; break; fi
    done
  fi
}

print_detection() {
  local line cap
  say "Detected system"
  info "OS:      $( (. /etc/os-release 2>/dev/null && echo "${PRETTY_NAME:-unknown}") || echo unknown)"
  info "CPU:     $(uname -m), $(nproc 2>/dev/null || echo '?') threads"
  if command -v gcc >/dev/null 2>&1; then info "gcc:     $(gcc --version | head -1)"; fi

  if [ -n "$NV_GPUS" ]; then
    while IFS= read -r line; do
      [ -n "$line" ] || continue
      info "NVIDIA:  $line"
      cap="${line##*, }"
      if [[ "$cap" =~ ^[0-9]+\.[0-9]+$ ]] && awk -v c="$cap" 'BEGIN { exit !(c + 0 < 6.1) }'; then
        warn "This GPU (compute capability $cap) is older than 6.1; the CUDA miner does not support it."
      fi
    done <<< "$NV_GPUS"
  else
    info "NVIDIA:  no GPU/driver visible (nvidia-smi missing or failing)"
    if command -v lspci >/dev/null 2>&1 \
       && lspci 2>/dev/null | grep -Ei 'vga|3d|display' | grep -Ei 'amd|ati|radeon|intel' >/dev/null; then
      info "An AMD/Intel GPU was found. There is no OpenCL/ROCm backend, so this machine will mine on the CPU."
    fi
  fi

  if [ -n "$CUDA_HOME_DIR" ]; then
    info "CUDA:    $CUDA_HOME_DIR ($("$CUDA_HOME_DIR/bin/nvcc" --version 2>/dev/null | tail -1 || echo 'nvcc version unknown'))"
  else
    info "CUDA:    no toolkit (nvcc) found"
  fi

  if [ -n "$NV_GPUS" ] && [ -n "$CUDA_HOME_DIR" ]; then
    info "Plan:    build with GPU (CUDA) support"
  elif [ -n "$NV_GPUS" ] && [ "$INSTALL_CUDA" -eq 1 ]; then
    info "Plan:    install nvidia-cuda-toolkit, then build with GPU support"
  elif [ -n "$NV_GPUS" ]; then
    info "Plan:    CPU-only build (NVIDIA GPU found but no CUDA toolkit). Re-run with --install-cuda, or install the CUDA toolkit first."
  elif [ -n "$CUDA_HOME_DIR" ]; then
    info "Plan:    build with CUDA support compiled in; no GPU is visible, so it will mine on the CPU until one is"
  else
    info "Plan:    CPU-only build"
  fi
}

install_packages() {
  say "Build dependencies"
  # Only ask for administrator rights when something is really missing. Packages already
  # installed system-wide (for example by another user of this machine) are reused.
  local missing=() p
  for p in "${PKGS[@]}"; do
    dpkg-query -W -f='${Status}' "$p" 2>/dev/null | grep -q "install ok installed" || missing+=("$p")
  done
  if [ "${#missing[@]}" -eq 0 ]; then
    info "All required system packages are already installed; skipping apt."
    return 0
  fi
  info "Missing packages: ${missing[*]}"
  if [ -n "$SUDO" ] && ! $SUDO -v; then
    die "This user cannot use sudo, so the missing packages cannot be installed.
    Ask an administrator, or from an admin account run:
        sudo apt-get install -y ${missing[*]}
    then run this installer again as this user (it will skip this step)."
  fi
  # shellcheck disable=SC2086
  $SUDO apt-get update
  # shellcheck disable=SC2086
  $SUDO env DEBIAN_FRONTEND=noninteractive apt-get install -y "${missing[@]}"
}

maybe_install_cuda() {
  if [ "$INSTALL_CUDA" -eq 1 ] && [ -n "$NV_GPUS" ] && [ -z "$CUDA_HOME_DIR" ]; then
    say "Installing the distribution CUDA toolkit (nvidia-cuda-toolkit)"
    warn "The distribution toolkit can be older than NVIDIA's own releases; that is fine for sm_61/75/86."
    # shellcheck disable=SC2086
    $SUDO env DEBIAN_FRONTEND=noninteractive apt-get install -y nvidia-cuda-toolkit
    detect_cuda
    if [ -z "$CUDA_HOME_DIR" ]; then warn "nvcc still not found after installing the toolkit; the build will be CPU-only."; fi
  fi
}

install_rust() {
  say "Rust toolchain"
  if ! command -v cargo >/dev/null 2>&1 && [ -f "$HOME/.cargo/env" ]; then
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
  fi
  if ! command -v cargo >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
  fi

  local ver
  ver="$(rustc --version | awk '{print $2}')"
  info "rustc $ver"
  if [ "$(printf '%s\n1.75.0\n' "$ver" | sort -V | head -1)" != "1.75.0" ]; then
    die "Rust $ver is too old (need 1.75+). If rustup is installed run 'rustup update stable' and re-run this installer."
  fi
}

latest_tag() {
  git ls-remote --tags --refs "$REPO_URL" 2>/dev/null \
    | sed 's|.*refs/tags/||' | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | sort -V | tail -1 || true
}

resolve_tag() {
  if [ -z "$TAG" ]; then
    TAG="$(latest_tag)"
    [ -n "$TAG" ] || die "Could not determine the newest release tag (network problem?). Pass one with --tag vX.Y.Z."
  fi
}

fetch_source() {
  say "Fetching Q-BTC Core $TAG"
  if [ -d "$DIR/.git" ]; then
    if ! git -C "$DIR" diff --quiet || ! git -C "$DIR" diff --cached --quiet; then
      die "$DIR has local changes. Commit or stash them, or choose another folder with --dir."
    fi
    git -C "$DIR" fetch --tags --force origin
  elif [ -e "$DIR" ]; then
    die "$DIR exists but is not a git checkout. Choose another folder with --dir."
  else
    git clone "$REPO_URL" "$DIR"
  fi
  git -C "$DIR" rev-parse -q --verify "refs/tags/$TAG^{commit}" >/dev/null || die "Tag $TAG not found in $REPO_URL"
  git -C "$DIR" -c advice.detachedHead=false checkout --quiet "$TAG"
  info "$(git -C "$DIR" log -1 --oneline)"
}

build() {
  say "Building (this takes several minutes)"
  cd "$DIR"
  # librocksdb-sys fails on newer GCC without this include.
  export CXXFLAGS="${CXXFLAGS:-} -include cstdint"
  # Release profile ships full debuginfo (~440 MB binary); a miner does not need it.
  export CARGO_PROFILE_RELEASE_DEBUG=false
  if [ -n "$CUDA_HOME_DIR" ]; then export CUDA_PATH="$CUDA_HOME_DIR"; fi

  if ! cargo build --release 2>&1 | tee "$DIR/build.log"; then
    die "Build failed. Details are in $DIR/build.log. If it complains about the Rust version, run 'rustup update stable' and retry."
  fi
  [ -x "$DIR/target/release/quantum-btc" ] || die "Build finished but target/release/quantum-btc is missing."
}

# build.rs reports GPU support only as a cargo warning, and cargo prints it only
# when the build script actually ran. Look in the build log first, then in the
# stored build-script output (covers cached builds).
check_gpu_build() {
  local out f
  out="$(ls -t "$DIR"/target/release/build/quantum-btc-*/output 2>/dev/null | head -1 || true)"
  GPU_BUILD="unknown"
  for f in "$DIR/build.log" "$out"; do
    [ -n "$f" ] && [ -f "$f" ] || continue
    if grep -q "CUDA kernel compiled" "$f"; then GPU_BUILD="yes"; break
    elif grep -q "CUDA build failed" "$f"; then GPU_BUILD="failed"; grep -E 'CUDA build failed|^warning: quantum-btc|cargo:warning' "$f" | head -25 >&2 || true; break
    elif grep -q "No CUDA toolkit found" "$f"; then GPU_BUILD="no"; break
    fi
  done

  case "$GPU_BUILD" in
    yes)    info "GPU support: compiled in." ;;
    failed) warn "The CUDA kernel failed to compile, so this binary mines on the CPU only."
            warn "A common cause is a gcc newer than your CUDA toolkit supports. Install an older g++ that your toolkit supports and re-run, e.g.:"
            warn "  NVCC_PREPEND_FLAGS='-ccbin g++-12' bash install.sh" ;;
    no)     info "GPU support: not compiled in (no CUDA toolkit found). CPU mining only." ;;
    *)      warn "Could not tell whether GPU support was compiled in. Run 'qbtc start' and look for 'CUDA device ... ready' in the first log lines." ;;
  esac

  if [ "$GPU_BUILD" = "yes" ]; then
    local ld="$CUDA_LIB_DIR" missing
    if [ -n "${LD_LIBRARY_PATH:-}" ]; then ld="${ld:+$ld:}$LD_LIBRARY_PATH"; fi
    missing="$(LD_LIBRARY_PATH="$ld" ldd "$DIR/target/release/quantum-btc" 2>/dev/null | grep 'not found' || true)"
    if [ -n "$missing" ]; then warn "Missing shared libraries:"; printf '%s\n' "$missing" >&2; fi
  fi
}

write_launcher() {
  local bin_dir="$HOME/.local/bin" backend=""
  if [ "$GPU_BUILD" = "yes" ]; then backend="cuda"; fi
  mkdir -p "$bin_dir"

  {
    printf '%s\n' '#!/usr/bin/env bash'
    printf '%s\n' '# Generated by the Q-BTC Linux installer. Re-run the installer to regenerate.'
    printf 'QBTC_HOME=%q\n' "$DIR"
    printf 'QBTC_DATADIR_DEFAULT=%q\n' "$DATADIR"
    printf 'QBTC_PORT_DEFAULT=%q\n' "$PORT"
    printf 'CUDA_LIB_DIR=%q\n' "$CUDA_LIB_DIR"
    printf 'DEFAULT_BACKEND=%q\n' "$backend"
    printf 'MONITOR_FILE=%q\n' "$MONITOR_FILE"
    printf 'MONITOR_URL=%q\n' "$MONITOR_URL"
  } > "$bin_dir/qbtc"

  cat >> "$bin_dir/qbtc" <<'LAUNCHER'
set -euo pipefail

QBTC_DATADIR="${QBTC_DATADIR:-$QBTC_DATADIR_DEFAULT}"
QBTC_PORT="${QBTC_PORT:-$QBTC_PORT_DEFAULT}"
SESSION="qbtc"
BIN="$QBTC_HOME/target/release/quantum-btc"

usage() {
  cat <<'U'
Usage: qbtc <command>

  start    Start the node in a tmux session (or re-attach if it is already running)
  attach   Attach to the running node session (detach with Ctrl+b, then d)
  status   Show whether the node is running and what the miner reported
  scrub    Clear the screen and scrollback of EVERY tmux window of the node (use after wallet_gen / wallet_restore)
  monitor  Live dashboard: checks that your miner is on the network's chain. Run it in a SECOND terminal
           (options: --address qbtc1... --tz Asia/Kolkata, see 'qbtc monitor --help')
  monitor-update   Download the newest dashboard

Mining and wallet commands are typed at the node prompt: see docs/COMMANDS.md
U
}

has_session() { tmux has-session -t "$SESSION" 2>/dev/null; }

cmd_attach() {
  if ! has_session; then echo "No '$SESSION' session is running. Use: qbtc start" >&2; exit 1; fi
  if [ -n "${TMUX:-}" ]; then exec tmux switch-client -t "$SESSION"; fi
  exec tmux attach -t "$SESSION"
}

cmd_start() {
  command -v tmux >/dev/null 2>&1 || { echo "tmux is not installed." >&2; exit 1; }
  [ -x "$BIN" ] || { echo "Binary not found: $BIN" >&2; exit 1; }

  if has_session; then
    echo "Session '$SESSION' already exists."
    cmd_attach
  fi
  if pgrep -x quantum-btc >/dev/null 2>&1; then
    echo "A quantum-btc process is already running outside the '$SESSION' tmux session." >&2
    echo "Not starting a second node. Stop the running one first." >&2
    exit 1
  fi

  mkdir -p "$QBTC_DATADIR"
  local backend="${QBTC_MINER_BACKEND:-$DEFAULT_BACKEND}"
  local ld="$CUDA_LIB_DIR"
  if [ -n "${LD_LIBRARY_PATH:-}" ]; then ld="${ld:+$ld:}$LD_LIBRARY_PATH"; fi

  local run
  run="$(printf 'env QBTC_MINER_BACKEND=%q LD_LIBRARY_PATH=%q %q --datadir %q --port %q' \
    "$backend" "$ld" "$BIN" "$QBTC_DATADIR" "$QBTC_PORT")"
  run="$run; echo; echo '[qbtc] node exited. Press Enter to close.'; read -r _"

  if [ -n "${TMUX:-}" ]; then
    tmux new-session -d -s "$SESSION" -c "$QBTC_HOME" "$run"
    echo "Started in the background. Switch to it with: tmux switch-client -t $SESSION"
  else
    exec tmux new-session -s "$SESSION" -c "$QBTC_HOME" "$run"
  fi
}

cmd_status() {
  if has_session; then echo "tmux session '$SESSION': present"; else echo "tmux session '$SESSION': not found"; fi
  if pgrep -x quantum-btc >/dev/null 2>&1; then
    echo "node process: running (pid $(pgrep -x quantum-btc | head -1))"
  else
    echo "node process: not running"
  fi
  local log
  log="$(ls -t "$QBTC_HOME"/logs/qbtc_node.log.* 2>/dev/null | head -1 || true)"
  if [ -n "$log" ]; then
    echo "log file: $log"
    grep -E 'backend selected|CUDA device|mining on' "$log" | tail -5 || true
    grep 'MINING' "$log" | tail -1 || true
  fi
}

cmd_scrub() {
  if has_session; then
    # Every pane of every window: a plain "-t qbtc" only reaches the ACTIVE pane,
    # which is the wrong one when the node runs in window 0 and you are in window 1.
    local p
    for p in $(tmux list-panes -s -t "$SESSION" -F '#{pane_id}'); do
      tmux send-keys -R -t "$p"
      tmux clear-history -t "$p"
    done
    echo "Cleared the screen and scrollback of every pane in tmux session '$SESSION'."
    echo "Check: attach (qbtc attach), press Ctrl+b then [ and scroll up; the seed phrase must be gone (q to leave)."
    echo "If the node prompt looks empty, press Enter inside the node window."
  else
    echo "No '$SESSION' tmux session found."
  fi
  echo "Also run 'clear' in this terminal and close any other window or tool that showed the phrase."
}

cmd_monitor() {
  command -v python3 >/dev/null 2>&1 || { echo "python3 is not installed (sudo apt-get install -y python3)." >&2; exit 1; }
  if [ ! -f "$MONITOR_FILE" ]; then
    echo "The dashboard is not installed yet. Run: qbtc monitor-update" >&2
    exit 1
  fi
  exec python3 "$MONITOR_FILE" "$@"
}

cmd_monitor_update() {
  mkdir -p "$(dirname "$MONITOR_FILE")"
  if curl -fsSL "$MONITOR_URL" -o "$MONITOR_FILE.new" && head -1 "$MONITOR_FILE.new" | grep -q python; then
    mv "$MONITOR_FILE.new" "$MONITOR_FILE"
    echo "Dashboard updated: $MONITOR_FILE"
  else
    rm -f "$MONITOR_FILE.new"
    echo "Download failed from $MONITOR_URL" >&2
    exit 1
  fi
}

case "${1:-}" in
  start)  cmd_start ;;
  attach) cmd_attach ;;
  status) cmd_status ;;
  scrub)  cmd_scrub ;;
  monitor) shift; cmd_monitor "$@" ;;
  monitor-update) cmd_monitor_update ;;
  "")     usage ;;
  -h|--help|help) usage ;;
  *)      echo "Unknown command: $1" >&2; echo >&2; usage >&2; exit 1 ;;
esac
LAUNCHER

  chmod +x "$bin_dir/qbtc"
  info "Launcher installed: $bin_dir/qbtc"

  # A script started with `curl | bash` cannot change the PATH of the terminal it was
  # started from, so make `qbtc` reachable in the three ways that are possible.
  if [ "$(id -u)" -eq 0 ] && [ -d /usr/local/bin ] && [ -w /usr/local/bin ]; then
    ln -sf "$bin_dir/qbtc" /usr/local/bin/qbtc && info "Also linked: /usr/local/bin/qbtc (works in this terminal right away)"
  fi
  local rc="$HOME/.bashrc"
  if [ -f "$rc" ] || [ -w "$HOME" ]; then
    if ! grep -qs '# added by the Q-BTC installer' "$rc"; then
      printf '\n# added by the Q-BTC installer\nexport PATH="$HOME/.local/bin:$PATH"\n' >> "$rc"
      info "Added ~/.local/bin to PATH in $rc (new terminals pick it up)"
    fi
  fi
}

# The dashboard lives outside the release tag (old tags do not contain it), so fetch it
# from the repository's main branch. Failure is not fatal.
install_monitor() {
  say "Installing the live dashboard"
  mkdir -p "$(dirname "$MONITOR_FILE")"
  if curl -fsSL "$MONITOR_URL" -o "$MONITOR_FILE.new" && head -1 "$MONITOR_FILE.new" | grep -q python; then
    mv "$MONITOR_FILE.new" "$MONITOR_FILE"
    info "Dashboard: $MONITOR_FILE   (run it with: qbtc monitor)"
  else
    rm -f "$MONITOR_FILE.new"
    warn "Could not download the dashboard from $MONITOR_URL. The node works without it; retry later with: qbtc monitor-update"
  fi
}

finish() {
  say "Done"
  info "Version: $TAG ($(git -C "$DIR" rev-parse --short HEAD))"
  info "Binary:  $DIR/target/release/quantum-btc ($(du -h "$DIR/target/release/quantum-btc" | cut -f1))"
  info "Data:    $DATADIR  (wallets live here; back up your seed phrases, not this folder)"
  if [ "$GPU_BUILD" = "yes" ]; then info "Mining:  GPU (CUDA) backend selected by the launcher"; else info "Mining:  CPU backend"; fi
  local qcmd="qbtc"
  if ! command -v qbtc >/dev/null 2>&1; then qcmd='export PATH="$HOME/.local/bin:$PATH" && qbtc'; fi
  cat <<EOF

==============================  NEXT STEPS  ==============================
 1. START THE NODE (it runs inside tmux, so it keeps running if you close the window)

        $qcmd start

    You now see the node's own screen. This is the node console: you type node
    commands here, nothing else. Wait until syncing has finished
    (first start takes a few minutes).

 2. CREATE A WALLET and START MINING (type these in that node screen)

        wallet_gen mywallet          <- write the 12 words on PAPER
        auto_mine start mywallet

    Then, from a SECOND terminal, wipe the seed phrase from the screen:   qbtc scrub
    Type 'balance mywallet' in the node screen to see your wallet address (qbtc1...).

 3. LEAVE THE NODE RUNNING:  press Ctrl+b, release, then press d   (= detach)
    Come back later with:    qbtc attach
    Scroll up in tmux:       Ctrl+b then [   (arrow keys / PgUp, q to leave)
    Status without attaching: qbtc status

 4. WATCH IT with the live dashboard (open a NEW terminal, do not stop the node):

        qbtc monitor --address qbtc1...your-address... --tz Asia/Kolkata

    The big coloured bar at the top tells you if your miner is on the SAME chain as
    the network (green) or wasting power on a stale/wrong chain (yellow/red).
    Keys: q quit, u UTC/local, r refresh, p pause.
    Only one terminal available? Press Ctrl+b then c (new tmux window), run the
    dashboard there; switch windows with Ctrl+b then 0 or 1.

 5. STOP: type 'auto_mine stop', then press Ctrl+C in the node screen and wait for it to exit.

Commands and seed-phrase safety: $DIR/docs/COMMANDS.md (if present in this release)
==========================================================================
EOF
}

main() {
  parse_args "$@"
  preflight
  detect_gpu
  detect_cuda
  print_detection

  if [ "$CHECK_ONLY" -eq 1 ]; then
    if command -v git >/dev/null 2>&1; then
      local t="${TAG:-$(latest_tag)}"
      if [ -n "$t" ]; then info "Tag:     $t"; else info "Tag:     could not be determined (offline?)"; fi
    fi
    say "--check: nothing was changed."
    exit 0
  fi

  if [ "$(id -u)" -ne 0 ]; then
    command -v sudo >/dev/null 2>&1 || die "Run as root, or install sudo."
    SUDO="sudo"
  fi

  install_packages
  maybe_install_cuda
  install_rust
  resolve_tag
  fetch_source
  build
  say "Checking the build"
  check_gpu_build
  write_launcher
  install_monitor
  finish
}

# Everything runs inside main() so a truncated download cannot execute half a script.
if [ -z "${QBTC_SOURCE_ONLY:-}" ]; then
  main "$@"
fi
