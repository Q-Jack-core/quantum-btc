# Node commands, wallets and mining

Everything you can type at the node prompt in v2.6.0, plus the start-up flags and environment variables. Examples use placeholder names and addresses.

## Starting the node

```bash
qbtc start                       # if installed with scripts/install.sh
# or, by hand:
QBTC_MINER_BACKEND=cuda ./target/release/quantum-btc --datadir ~/.qbtc --port 8000
```

The node must finish **syncing** before it will mine. While it syncs, `auto_mine start` is refused with `Auto-mining is physically locked during IBD`.

## Commands at the node prompt

| Command | What it does | Example |
|---|---|---|
| `wallet_gen <name>` | Create a wallet. Asks for a password (nothing is echoed while you type) and shows a 12-word seed phrase **once** | `wallet_gen mywallet` |
| `wallet_restore <name> <12 words>` | Recreate a wallet from its seed phrase. The words are typed on the command line, so read [Seed phrase safety](#seed-phrase-safety) first | `wallet_restore mywallet w1 w2 w3 w4 w5 w6 w7 w8 w9 w10 w11 w12` |
| `wallet_change_password <name>` | Change a wallet's password | `wallet_change_password mywallet` |
| `list_wallets` | List the wallets in the data directory | `list_wallets` |
| `balance <name>` | Show Confirmed, Pending (Mempool) and Locked (Coinbase) amounts | `balance mywallet` |
| `transfer <amount> <address> <name>` | Send coins from a wallet. Asks for the wallet password | `transfer 10 qbtc1exampleaddress... mywallet` |
| `auto_mine start <wallet or address>` | Mine continuously; block rewards go to that wallet or address | `auto_mine start mywallet` |
| `auto_mine stop` | Stop mining | `auto_mine stop` |
| `mine` | Listed in the node's command list as a mining command | `mine` |
| `connect <ip> <port>` | Connect to a specific peer | `connect 203.0.113.5 8000` |

Notes:

- **Always give `auto_mine start` a target.** If the name does not match a wallet or a valid address the node answers `Target ... does not exist` and mining does not start, so rewards cannot go to a wrong place by accident.
- **Coinbase maturity is 100 blocks.** New block rewards show as *Locked (Coinbase)* and become *Confirmed* once 100 more blocks have been mined.
- **Transfers** appear as *Pending (Mempool)* first and *Confirmed* after they are in a block. Network fees apply; if sending your entire balance is rejected, try slightly less.
- **Adding a wallet** means `wallet_gen` (new) or `wallet_restore` (from a seed phrase). There is no `wallet_remove` command in v2.6.0. Wallet files live in the `keystores` folder inside the data directory (`<datadir>/keystores`); the `vault` folder next to it is the chain database. To stop using a wallet, leave it alone. Do not delete anything from `keystores` unless the seed phrase is backed up, because without it the funds cannot be recovered. Copying `keystores` is not a substitute for the seed phrase.

### Stopping cleanly

```text
auto_mine stop
```

then press `Ctrl+C` in the node's terminal and wait for it to exit. Avoid `kill -9`.

## Start-up flags

| Flag | Meaning |
|---|---|
| `--datadir <path>` | Data and wallet folder. Default `./.qbtc_data`, which is relative to the folder you start the binary from, so start it from the same place every time (the `qbtc` launcher uses `~/.qbtc`) |
| `--port <port>` | P2P port (default `8000`). The RPC server uses port + 1 and should not be exposed publicly |
| `--rescan`, `-reindex-chainstate` | Rebuild chain state |
| `--enable-explorer-api` | Enable the explorer API |

## Environment variables

| Variable | Meaning |
|---|---|
| `QBTC_MINER_BACKEND` | `cpu` (default), `cuda` / `gpu`, or `hybrid` / `both` / `cpu+gpu`. If unset, the node mines on the CPU |
| `QBTC_CUDA_DEVICES` | Comma-separated GPU ids to use, e.g. `0,1`. Default: all |
| `QBTC_CUDA_BATCH_SIZE`, `QBTC_CUDA_BLOCKS`, `QBTC_CUDA_THREADS` | GPU tuning (see [GPU_MINING.md](GPU_MINING.md)) |
| `QBTC_CUDA_DEBUG` | Extra GPU debug logging |
| `QBTC_RPC_TOKEN`, `QBTC_RPC_ALLOWED_HOSTS`, `QBTC_RPC_CORS_ORIGINS` | RPC access controls |

## Watching the miner

The log file rotates daily under `logs/` in the folder the node was started from:

```bash
tail -f logs/qbtc_node.log.$(date +%F) | grep --line-buffered MINING          # hashrate
grep 'CUDA device' logs/qbtc_node.log.$(date +%F)                              # which GPUs came up
watch -n 5 nvidia-smi                                                           # GPU load, every 5 s
```

A mined block shows up as lines like `Proof-of-Work puzzle solved`, then `Local block physically secured and broadcasted`.

## tmux basics

The node runs in a tmux session so it keeps mining after you close the terminal.

| Keys / command | What it does |
|---|---|
| `Ctrl+b` then `d` | Detach (the node keeps running) |
| `qbtc attach` or `tmux attach -t qbtc` | Re-attach |
| `Ctrl+b` then `[` | Scroll mode: arrows / PgUp / PgDn scroll, `q` exits. The screen looks frozen while you are in it; press `q` |
| `Ctrl+b` then `c` / `n` / `p` | New window / next / previous |

## Seed phrase safety

Your 12-word seed phrase is the only way to recover a wallet, and anyone who sees it can take the funds. The node shows it on screen, and **a terminal keeps what it has shown**.

### Creating a wallet

1. Run `wallet_gen <name>`, set a password and write the 12 words on **paper**. No screenshots, no cloud notes, no chat messages, no photos.
2. Do it somewhere nobody can see the screen. Do not record the session (no `script`, no `tmux pipe-pane` logging, no screen sharing).
3. Immediately clear the screen **and** the scrollback:
   - With the launcher, from a second terminal: `qbtc scrub` (clears every window and pane of the node session)
   - By hand (all windows and panes of the session):
     `for p in $(tmux list-panes -s -t qbtc -F '#{pane_id}'); do tmux send-keys -R -t "$p"; tmux clear-history -t "$p"; done`
   - In a plain terminal without tmux: `clear` and `printf '\033[3J'`, then close the window
4. Check it worked: press `Ctrl+b` then `[` inside the session and scroll up. If you can still see the words, repeat step 3.

The node itself cannot erase what the terminal has already shown, so step 3 is a manual step.

### Restoring a wallet

`wallet_restore` takes the words as part of the command line, so they appear on screen and in the scrollback. Do the same clean-up as above straight afterwards.

Commands typed inside the node prompt are not part of your shell history. If you ever typed the words at the **shell** prompt by mistake, remove them: run `history | tail`, then `history -d <line number>` and `history -w`.

### Rented or shared machines (RunPod and similar)

- Treat the machine as untrusted. Do **not** restore a wallet that holds real funds there.
- Create a throwaway wallet with `wallet_gen`, mine to it, and move the rewards to your own wallet with `transfer` regularly. Only mature (Confirmed) funds can be moved.
- Terminating the machine destroys its disk and every key on it. Back up the seed phrase of anything you intend to keep before you do.
- If you ever paste a terminal log into a chat, an issue or a pull request, check it for seed words and addresses first.

### Quick checklist

- [ ] Seed phrase written on paper, stored safely
- [ ] Screen and scrollback cleared (`qbtc scrub`) and verified
- [ ] Wallet password is not the seed phrase and is stored separately
- [ ] Real funds are not sitting on a rented machine
