# Quantum Bitcoin (Q-BTC) Core

*Read this in other languages: [English](README.md), [简体中文](README_zh.md).*

[![Rust](https://img.shields.io/badge/rust-1.75%2B-blue.svg)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Status: Mainnet Live](https://img.shields.io/badge/Status-Mainnet_Live-green.svg)]()

> "17/May/2026: The quantum age dawns. The 21,000,000 truth shines eternal."

Quantum Bitcoin (Q-BTC) is a post-quantum peer-to-peer electronic cash system built from the ground up in Rust. It eliminates the elliptic curve (ECDSA) vulnerabilities exposed by scalable quantum computing, natively integrating the NIST-standardized ML-DSA-65 (Module Lattice-Based Digital Signature Standard) at the base consensus layer.

---

## 🚀 Quick Start: Join the Network

You don't need to be a developer to support the network. Our pre-compiled binaries make it easy to participate immediately.

### 1. Download Your Mining Engine
Visit the [Releases page](https://github.com/Q-Jack-core/quantum-btc/releases) and download the file for your system:

- Windows: Download qbtc-core-windows.zip
- macOS: Download qbtc-core-macos.tar.gz
- Linux: Download qbtc-core-linux.tar.gz

### 2. Launch the Engine

**For Windows:**
```text
.\quantum-btc.exe
```

**For macOS/Linux:**
```text
./quantum-btc
```

### 3. Generate Wallet & Start Mining
**Important: After launching the node for the first time, please wait a few minutes for the network synchronization to complete.** The node needs to download and verify the blockchain history before you can interact with it.

Once the node is online and fully synchronized, follow these two commands to participate:

Step A: Create your wallet (replace mywallet with your chosen name):
```text
wallet_gen mywallet
```
*After running this command, follow the on-screen prompts to securely enter and confirm your password.* (Save your 12-word seed phrase securely. It is the only way to recover your assets.)

Step B: Start automated mining:
```text
auto_mine start mywallet
```
You will see the node begin processing hash iterations immediately. Your computer is now actively securing the Q-BTC network!

---

## 🛡️ Core Philosophy: A Grassroots Experiment
Quantum BTC is a 100% fair-launch, pure Proof-of-Work chain. There is **zero VC funding, no pre-mine, no dev tax, and no corporate backing**. It is a purely grassroots cypherpunk experiment designed to test anti-quantum cryptography in the wild.

Permissionless means exactly that. Whether you are pointing a massive server farm at us, running a single CPU node on a dusty ThinkPad, or you are a developer trying to break our code—you are welcome here.

---

## 🏛️ Architecture Deep Dive

*For developers and security researchers wishing to inspect the protocol:*

Q-BTC is a fundamental architectural redesign optimized for the post-quantum era, including:

1. Actor-Driven State Machine: Eradicates Mutex locks in the UTXO set using an OS-exclusive UtxoActor model.
2. PQ-SegWit Pruning: Counters ML-DSA-65 signature bloat via physical bifurcation of Core state and Witness data.
3. Topological DAG Mempool: Defends against high-frequency Replay Floods using strict Directed Acyclic Graph (DAG) structures.
4. Compact Blocks (Q-BIP-152): Native integration of compact routing to eliminate lattice-induced bandwidth congestion.
5. Parallel Validation: Shatters single-thread limits using rayon for concurrent ML-DSA matrix multiplications.
6. Layer-1 Delayed Recovery: Native disaster recovery embedding RecoveryInfo within TxOut to preserve a 30-day veto window.
7. Monolithic Adherence: Fiercely defending the Monolithic architecture while laying groundwork for future ZK-STARK evolution.

---

## ⚙️ Building from Source

### Prerequisites
- Rust toolchain (cargo 1.75+)
- CMake & Clang (for RocksDB compilation)

### Build Instructions

```bash
git clone https://github.com/Q-Jack-core/quantum-btc.git
cd quantum-btc
cargo build --release
```

### Run the Node

After building from source, you can start the Q-BTC node directly:

```bash
cargo run --release
```

*(Alternatively, you can execute the compiled binary: `./target/release/quantum-btc`)*

---

## ⚠️ Disclaimer
**The Q-BTC mainnet has successfully crossed the 10,000-block genesis stress phase and is now entering wild hash-rate expansion.** While the post-quantum baseline has been practically validated, this remains a hardcore cryptography experiment. This software is provided "as-is" without commercial promises or VC bailouts. Participate, secure the network, and bear the risks at your own discretion.

📄 **[Read the Official Genesis Whitepaper V2.0](./Q-BTC%20Whitepaper%20V2.0.pdf)**