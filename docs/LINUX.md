# Linux setup guide

Run a Q-BTC node and miner on Debian/Ubuntu, with or without an NVIDIA GPU.

## One-line install

```bash
curl -fsSL https://raw.githubusercontent.com/Q-Jack-core/quantum-btc/main/scripts/install.sh | bash
```

Prefer to read it first (recommended):

```bash
curl -fsSLO https://raw.githubusercontent.com/Q-Jack-core/quantum-btc/main/scripts/install.sh
less install.sh
bash install.sh
```

Pin a release instead of taking the newest tag:

```bash
bash install.sh --tag v2.6.0
```

See what the installer detects without changing anything:

```bash
bash install.sh --check
```

## What the installer does

1. Checks OS, CPU architecture, RAM, free disk, NVIDIA GPU and CUDA toolkit.
2. Installs build dependencies with `apt` and Rust with `rustup`.
3. Clones this repository and checks out a release tag (newest `vX.Y.Z` unless you pass `--tag`).
4. Runs `cargo build --release`. `build.rs` compiles the CUDA kernel if a toolkit is found and otherwise builds a CPU stub (see [GPU_MINING.md](GPU_MINING.md)).
5. Reports whether the binary was built **with GPU support**, because the build never fails when CUDA is missing; it silently becomes CPU-only.
6. Installs a launcher, `qbtc`, into `~/.local/bin`. It is made reachable at once (symlink in `/usr/local/bin` when run as root) and in new terminals (PATH line in `~/.bashrc`). A script run through `curl | bash` cannot change the PATH of the terminal it was started from; if `qbtc` is still not found, run `export PATH="$HOME/.local/bin:$PATH"` once.
7. Downloads the live dashboard to `~/.local/share/qbtc/qbtc-monitor.py` (used by `qbtc monitor`).

It does not create wallets, does not ask for or handle seed phrases, and does not modify an existing data directory.

## Options

| Option | Meaning |
|---|---|
| `--tag <vX.Y.Z>` | Release tag to build (default: newest) |
| `--dir <path>` | Source checkout (default `~/quantum-btc`) |
| `--datadir <path>` | Node data and wallet folder (default `~/.qbtc`) |
| `--port <port>` | P2P port (default `8000`; RPC uses port + 1) |
| `--install-cuda` | If an NVIDIA GPU is present but `nvcc` is missing, install the distribution package `nvidia-cuda-toolkit` first |
| `--check` | Report only |

Environment overrides: `QBTC_TAG`, `QBTC_DIR`, `QBTC_DATADIR`, `QBTC_PORT`, `QBTC_REPO`, `CUDA_PATH`.

## What it works on

| System | Result |
|---|---|
| Debian/Ubuntu, x86_64, no GPU | Builds and mines on the CPU |
| NVIDIA GPU (compute capability 6.1+, i.e. GTX 10-series and newer), NVIDIA driver installed, CUDA toolkit installed | GPU build |
| NVIDIA GPU + driver, no CUDA toolkit | CPU-only build. Add `--install-cuda`, or install the CUDA toolkit yourself and re-run |
| NVIDIA GPU older than compute capability 6.1 | Not supported by the CUDA kernel |
| AMD / Intel GPU | No GPU backend (no OpenCL/ROCm). The node builds and mines on the CPU |
| Fedora, Arch and other non-apt distributions | The installer stops with a message; follow [Manual install](#manual-install) |
| ARM64 | Untested (CPU only at best) |
| WSL2 | Untested |
| Windows / macOS | Not this script; use the Desktop GUI or the `Start_Mining` launchers in the README |

The NVIDIA **driver** is never installed by this script. `nvidia-smi` must already work.

CUDA code is built for `sm_61`, `sm_75`, `sm_86` plus PTX for `compute_86`, so newer GPUs rely on driver JIT. Details and tuning: [GPU_MINING.md](GPU_MINING.md).

## After installing

```bash
qbtc start        # node in a tmux session; detach with Ctrl+b then d
qbtc attach       # come back to it
qbtc status       # process, backend and last hashrate line
qbtc scrub        # clear every tmux window/pane of the node and its scrollback (see COMMANDS.md)
qbtc monitor      # live dashboard, run it in a second terminal (see below)
```

Then, at the node prompt, after the node has finished syncing:

```text
wallet_gen mywallet
auto_mine start mywallet
```

All commands, flags, environment variables and **seed phrase handling** are in [COMMANDS.md](COMMANDS.md).

On a GPU build, the launcher selects the CUDA backend. The first log lines should show:

```text
[INFO] Miner: CUDA backend selected
[INFO] Miner: CUDA device 0 ready: <your GPU>
[INFO] Miner: mining on 1 GPU(s): [0]
```

If you see `CPU backend selected` instead, the node is mining on the CPU.

## Live mining monitor (network-first)

`qbtc monitor` is a read-only, self-updating terminal view. Its job is to tell you, first of all,
whether your miner is working on the **same chain as the network**. Leave the node mining, open a
**new terminal** (or a second tmux window: `Ctrl+b` then `c`) and run:

```bash
qbtc monitor --address qbtc1...your-address... --tz Asia/Kolkata
```

Your address is shown by `balance <name>` at the node prompt. Without the helper:

```bash
python3 ~/.local/share/qbtc/qbtc-monitor.py --address qbtc1...
```

What the screen shows, in order of importance:

1. **Verdict bar** (large, coloured). Green: on the network chain. Yellow: behind or not verifiable.
   Red: stale block, ahead of the network, different difficulty or a different block hash at the same
   height (= your node is on another chain; stop mining and re-sync).
2. **Comparison**: network latest block vs your node's latest block, the block being mined vs the
   network's next block, and the difficulty target. A tick means they agree.
3. **Hashrate** in large digits, per GPU rate, temperature and load (`nvidia-smi`).
4. **Wallet balance** from the network (confirmed, pending, locked) when `--address` is given.
5. Small details: miner state, backend, node process (the process id is the number the operating
   system gives the running node; "NOT RUNNING" means the node has crashed or was stopped),
   network hashrate estimate, your share and expected blocks per hour, which data sources answer.
6. **Recent network blocks**: green = yours, red = others, yellow = a block your miner found that is
   not in the network chain.

Where the data comes from: block list, tip, difficulty and balance come from the **network** (the
explorer, the public pool API and your own node's RPC on `127.0.0.1`). The node log is used only for
local facts: hashrate, GPU, backend, the block being mined and the hashes of blocks your node found.
If the network cannot be reached the bar says so; it never shows green without evidence.

Keys: `q` quit, `u` switch UTC/local time, `r` refresh now, `p` pause.

Options: `--address qbtc1...`, `--wallet NAME` (label only), `--tz Asia/Kolkata`, `--utc`,
`--explorer URL`, `--pool-api URL`, `--rpc URL|auto|off`, `--offline` (no internet requests),
`--logdir PATH`, `--days 1`, `--interval 5`, `--net-interval 60`, `--balance-unit auto|qbtc|base`, `--once`, `--json`,
`--discover`, `--no-color`.

Notes:

- It refreshes every 5 seconds, asks the shared explorer and pool only about once a minute (`--net-interval`, minimum 30 s, with random jitter and back-off after errors; the screen and your own node are read every 5 s without any internet traffic), and runs at
  the lowest CPU priority, so it does not compete with the miner.
- Routes used (explorer and your node's RPC share them): `GET /api/get_info`, `POST /api/get_block {"height": N}`
  and, on the explorer, `POST /api/get_balance {"address": "qbtc1..."}`. Blocks carry no hash field, so a block's
  hash is taken from the `previous_hash` of the next block, and two nodes' blocks are compared by parent,
  merkle root and nonce. Your own blocks are recognised by their nonce and by your address' key hash in the
  block's coinbase output. `--discover` prints what each route returns if the API ever changes.
- It never touches wallets, keys or seed phrases. `--address` is a public address, not a wallet name.
- The mining target is not written to the node log; the dashboard looks for it in the `qbtc` tmux
  session. If it cannot be found, pass `--address`.
- `qbtc monitor-update` downloads the newest version of the dashboard.

## Manual install

```bash
sudo apt-get update
sudo apt-get install -y build-essential clang libclang-dev cmake pkg-config libssl-dev git tmux curl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"

git clone https://github.com/Q-Jack-core/quantum-btc.git
cd quantum-btc
git checkout v2.6.0
export CXXFLAGS="-include cstdint"      # needed by librocksdb-sys on newer GCC
cargo build --release 2>&1 | tee build.log
grep -E "CUDA kernel compiled|No CUDA toolkit|CUDA build failed" build.log

QBTC_MINER_BACKEND=cuda ./target/release/quantum-btc --datadir ~/.qbtc
```

## Troubleshooting

**`No CUDA toolkit found ... Building without GPU support`**: `nvcc` was not found. Install the CUDA toolkit (or use `--install-cuda`), or set `CUDA_PATH`, then rebuild.

**`CUDA build failed; falling back to the CPU stub`**: the lines after it contain `nvcc`'s own error. A frequent cause is a `gcc` newer than your CUDA toolkit supports. Install an older `g++` that your toolkit supports and point `nvcc` at it, for example `NVCC_PREPEND_FLAGS='-ccbin g++-12' bash install.sh`.

**`Miner: no CUDA device could be initialized`**: the binary has GPU support but the driver sees no usable device. Check `nvidia-smi`.

**`error while loading shared libraries: libcudart.so`**: the CUDA runtime library is not on the loader path. The `qbtc` launcher sets it for toolkits under `/usr/local/cuda*`; if you start the binary by hand, add the toolkit's `lib64` directory to `LD_LIBRARY_PATH`.

**Build runs out of memory**: the release profile uses fat LTO. Add swap or build on a machine with more RAM.

**A rocksdb compile error mentioning `uint8_t` / `cstdint`**: build with `export CXXFLAGS="-include cstdint"` (the installer does this).

## Uninstall

```bash
rm -f ~/.local/bin/qbtc
rm -rf ~/quantum-btc          # source and build output
# Your data directory (default ~/.qbtc) holds your wallets. Delete it only if
# every wallet's seed phrase is safely backed up.
```

## Tested on

| Environment | GPU | Result |
|---|---|---|
| RunPod `runpod/pytorch:2.4.0-py3.11-cuda12.4.1-devel-ubuntu22.04` (manual steps above) | RTX 3090 | GPU build, mining, ~4.8 GH/s |
| _to be filled in with `install.sh` runs_ | | |
