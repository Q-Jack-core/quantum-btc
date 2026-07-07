# 量子比特币 (Q-BTC) 核心节点

[![Rust](https://img.shields.io/badge/rust-1.75%2B-blue.svg)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Status: Mainnet Live](https://img.shields.io/badge/Status-Mainnet_Live-green.svg)]()

> "17/May/2026: The quantum age dawns. The 21,000,000 truth shines eternal."

量子比特币 (Q-BTC) 是一个完全使用 Rust 从零构建的后量子点对点电子现金系统。它彻底消除了可扩展量子计算对传统椭圆曲线 (ECDSA) 造成的安全漏洞，并在底层共识层原生集成了 NIST 标准的 ML-DSA-65 (基于模块晶格的数字签名标准)。

---

## 🚀 快速开始：加入网络

您无需成为资深开发者也能支持本网络。通过我们预编译的二进制文件，您可以立即参与其中。

### 1. 下载您的挖矿引擎
访问 [Releases 页面](https://github.com/Q-Jack-core/quantum-btc/releases) 并下载适用于您系统的文件：

- Windows: 下载 `qbtc-core-windows.zip`
- macOS: 下载 `qbtc-core-macos.tar.gz`
- Linux: 下载 `qbtc-core-linux.tar.gz`

### 2. 启动引擎

**对于 Windows:**
```text
.\quantum-btc.exe
```

**对于 macOS/Linux:**
```text
./quantum-btc
```

### 3. 生成钱包并开始挖矿
**重要提示：首次启动节点后，请耐心等待几分钟以完成网络同步。** 节点需要下载并验证区块链历史记录，然后您才能与其交互。

当节点上线并完全同步后，请依次执行以下两条命令参与网络：

步骤 A：创建您的钱包（将 `mywallet` 替换为您选择的名称）：
```text
wallet_gen mywallet
```
*运行此命令后，请按照屏幕提示安全地输入并确认您的密码。*（请务必安全妥善地保存您的 12 词助记词。这是恢复您资产的唯一途径。）

步骤 B：启动自动挖矿：
```text
auto_mine start mywallet
```
您将看到节点立即开始处理哈希迭代。您的计算机现在正在积极捍卫 Q-BTC 网络的安全！

---

## 🛡️ 核心理念：一场草根极客实验
Q-BTC 是一条 100% 公平启动、纯粹的 Proof-of-Work (工作量证明) 公链。这里**零 VC 融资、无预挖、无开发者税、无企业背书**。这完全是一场草根级别的赛博朋克实验，旨在野外环境中测试抗量子密码学的极限。

“无需许可”就是字面意思。无论您是将庞大的服务器集群指向我们，还是在一台积灰的 ThinkPad 上运行单个 CPU 节点，抑或是试图攻破我们代码的开发者——这里都欢迎您。

---

## 🏛️ 架构深度解析

*供希望审查协议的开发者与安全研究人员参考：*

Q-BTC 针对后量子时代进行了底层的架构重构，包含：

1. **Actor 驱动的状态机**：采用操作系统独占的 `UtxoActor` 模型，彻底根除 UTXO 集中的 Mutex 锁机制。
2. **PQ-SegWit 裁剪**：通过 Core 状态与 Witness 数据的物理分叉，对抗 ML-DSA-65 签名带来的体积膨胀。
3. **拓扑 DAG 内存池**：使用严格的有向无环图 (DAG) 结构，防御高频重放洪水攻击。
4. **紧凑区块 (Q-BIP-152)**：原生集成紧凑路由，消除由晶格密码引发的带宽拥堵。
5. **并行验证**：使用 `rayon` 进行并发的 ML-DSA 矩阵乘法运算，打破单线程瓶颈。
6. **Layer-1 延迟恢复**：将 `RecoveryInfo` 原生嵌入 `TxOut` 中，保留 30 天的一票否决权窗口，实现底层灾备。
7. **坚守单体架构**：在死守单体架构底线的同时，为未来 ZK-STARK 的演进奠定坚实基础。

---

## ⚙️ 从源码构建

### 环境要求
- Rust 工具链 (cargo 1.75+)
- CMake & Clang (用于编译 RocksDB)

### 构建指令
```bash
git clone https://github.com/Q-Jack-core/quantum-btc.git
cd quantum-btc
cargo build --release
```

---

## ⚠️ 免责声明
**Q-BTC 主网已成功跨越 10,000 区块的创世高压期，现正式进入算力扩张阶段。** 尽管抗量子底层防线已得到初步的实盘验证，但这仍是一场极其硬核的前沿密码学实验。本软件“按原样”提供，无任何商业承诺或资本兜底。参与捍卫网络的风险与荣光，均由您自行承担。

📄 **[阅读官方创世白皮书 V2.0](./Q-BTC%20Whitepaper%20V2.0.pdf)**
