# GPU mining

The node ships an optional CUDA backend for the SHA-256d proof of work. It is
off by default: without it, and without a CUDA toolkit installed, the node
builds and mines on the CPU exactly as before.

## Requirements

- An NVIDIA GPU of compute capability 6.1 or newer (GTX 10-series onward).
- The CUDA toolkit. Prebuilt SASS is emitted for `sm_61`, `sm_75` and `sm_86`,
  plus PTX for `compute_86`, so newer architectures still run via driver JIT.
- On Windows, the MSVC C++ build tools (`nvcc` uses `cl.exe` as its host
  compiler). You do **not** need a Developer Command Prompt: the build script
  locates the toolchain itself.

There is no OpenCL or ROCm backend, so AMD and Intel GPUs are not supported.

## Building

```
cargo build --release
```

That is all. `build.rs` looks for a CUDA toolkit in this order:

1. `$CUDA_PATH`
2. Windows: the newest `C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\v*`
3. Linux: `/usr/local/cuda`, `/opt/cuda`, `/usr`, then the newest
   `/usr/local/cuda-*`

If none is found, or if `nvcc` fails, the build prints a `warning:` explaining
why and compiles a stub instead. The node still builds; GPU mining is simply
unavailable in that binary. Watch for one of these lines:

```
warning: CUDA kernel compiled; GPU mining available.
warning: No CUDA toolkit found (set CUDA_PATH to override). Building without GPU support.
```

## Running

Select the backend with `QBTC_MINER_BACKEND`:

| Value | Behaviour |
|---|---|
| unset, `cpu` | Multi-core CPU mining (default) |
| `cuda`, `gpu` | GPU only; falls back to CPU if no device initialises |
| `hybrid`, `both`, `cpu+gpu` | CPU pool and GPU run together on the same template |

```sh
# Linux / macOS
QBTC_MINER_BACKEND=cuda ./target/release/quantum-btc --datadir ./qbtc_data --port 8000
```

```powershell
# Windows
$env:QBTC_MINER_BACKEND = "cuda"
.\target\release\quantum-btc.exe --datadir .\qbtc_data --port 8000
```

`Start_Mining_GPU.bat` and `Start_Mining_GPU.command` do this for you, next
to the equivalent CPU launchers. They set the backend, clear any stale tuning
overrides, and tell you what to look for once mining starts.

On startup the miner reports which devices it is using:

```
[INFO] Miner: CUDA device 0 ready: NVIDIA GeForce GTX 1060 3GB
[INFO] Miner: mining on 1 GPU(s): [0]
```

## Watching it run

Each device reports progress every two seconds, in the console and in the
node's log file:

```
[MINING] GPU0 progress: 7700000000 hashes total | 784.5 MH/s | elapsed 362s | nonce=7800000000 | last_batch=0.127s
```

`last_batch` is the wall time of a single kernel invocation, including the
synchronise wait rather than pure kernel time. It is a useful baseline if you
are weighing up whether async streams would pay off, since the waste per
launch transition scales with it.

To follow it from another window, or to filter down to just the hashrate:

```sh
# Linux / macOS
tail -f logs/qbtc_node.log.$(date +%F)
tail -f logs/qbtc_node.log.$(date +%F) | grep --line-buffered MINING
```

```powershell
# Windows
$log = (Get-ChildItem .\logs\qbtc_node.log.* | Sort-Object Name | Select-Object -Last 1).FullName
Get-Content -Wait -Tail 20 $log
Get-Content -Wait $log | Select-String MINING
```

The file name carries a date suffix because the log rotates daily.

To check which devices came up without waiting for a progress line:

```sh
grep 'CUDA device' logs/qbtc_node.log.$(date +%F)
```

## Multiple GPUs

All visible devices are used by default. Each gets its own host thread and its
own disjoint slice of the nonce space: device *i* of *n* starts at
`i * batch_size` and advances by `batch_size * n`, so no two devices ever test
the same nonce. Threads advance independently, so mixed hardware needs no
coordination: a slower card simply completes fewer batches and never holds a
faster one back.

Restrict which devices are used with `QBTC_CUDA_DEVICES`:

```sh
QBTC_CUDA_DEVICES=0,2 QBTC_MINER_BACKEND=cuda ./target/release/quantum-btc ...
```

With one device this collapses to a stride of 1, which is the plain
single-GPU case: there is no separate code path for it.

## Tuning

| Variable | Default | Notes |
|---|---|---|
| `QBTC_CUDA_DEVICES` | all devices | Comma-separated device ids |
| `QBTC_CUDA_BATCH_SIZE` | `100000000` | Nonces per kernel invocation |
| `QBTC_CUDA_BLOCKS` | the device's SM count | Grid size |
| `QBTC_CUDA_THREADS` | `1024` | Threads per block |
| `QBTC_CUDA_DEBUG` | unset | Logs the CPU reference hash of the job header |

The defaults are deliberate: one 1024-thread block per SM. More resident warps
measured *slower* on Pascal, because the fully unrolled kernel is roughly 16 KB
of SASS and warps sitting at different program counters thrash the instruction
cache. On a GTX 1060 3GB: 16 to 32 warps/SM ~785 MH/s, 48 warps ~700, 64 warps
~695. Before changing `QBTC_CUDA_BLOCKS` or `QBTC_CUDA_THREADS`, measure.

Progress is logged every two seconds per device:

```
[MINING] GPU0 progress: 7700000000 hashes total | 784.5 MH/s | elapsed 362s | nonce=7800000000 | last_batch=0.127s
```

## Measured performance

Single GTX 1060 3GB (Pascal, 9 SMs, `sm_61`), 120 W board power limit:

| Stage | MH/s |
|---|---|
| Naive CUDA port | ~200 |
| + host-side precompute of all nonce-independent work | ~620 |
| + K+W precombination, midstate, final-round early exit | ~845 |

Sustained throughput on that card settles at roughly 785 MH/s rather than 845.

**Measurement conditions:** the card used for these numbers has a failing
cooler fan, so it thermally throttles under sustained load: the clock drops
from about 1950 MHz cold to about 1810 MHz once heat-soaked, and measured
throughput tracks core clock almost linearly (about 0.43 MH/s per MHz for this
kernel). The burst figure reflects a cold card and the sustained figure a
heat-soaked one. On a card with working cooling both numbers should be higher;
treat these as a conservative lower bound rather than a ceiling.

The kernel is ALU-bound (funnel shifts are half rate on `sm_61`), so
instruction count per nonce is the only lever that moves throughput. It
compiles to 32 registers with zero spills.

## Correctness

The GPU is treated as an untrusted accelerator. Every nonce it reports is
re-hashed and re-checked on the CPU through
`ConsensusEngine::verify_proof_of_work()` before the block can be published, so
a kernel bug can cost throughput but cannot produce an invalid block. A
rejected candidate is logged as a warning and mining continues.

The CPU path's midstate cache (`CpuPowHasher`) is likewise only a pre-filter:
anything it flags is confirmed against the authoritative
`Block::calculate_hash()`. It is covered by a unit test that compares it to a
plain SHA-256 of the assembled 120-byte header over 2000 randomised inputs:

```
cargo test --lib cpu_pow_hasher
```

## Troubleshooting

**`warning: CUDA build failed`**: the following `warning:` lines carry nvcc's
own error text. On Windows, a toolkit older than the installed MSVC toolset is
the usual cause; the build already passes
`-allow-unsupported-compiler`, so if it still fails, install a newer CUDA
toolkit or an older MSVC toolset.

**`Miner: no CUDA device could be initialized`**: the toolkit was found at
build time but the driver sees no usable device at run time. Check
`nvidia-smi`.

**`Miner: GPU0 has failed N batches in a row`**: a CUDA call keeps failing.
The code in the message identifies the stage: `-1` kernel launch or
synchronise, `-3`/`-4` a host-to-device copy, `-5` the result copy back,
`-6` a device id out of range, `-7` `cudaSetDevice`. Failed batches are not
counted toward the reported hashrate and the nonce range is retried rather
than skipped.

**Reported hashrate looks too good**: make sure you are reading the per-device
`[MINING] GPUn progress:` line. Failed batches are excluded from that figure by
design, so it reflects work actually completed.
