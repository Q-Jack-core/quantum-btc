#!/usr/bin/env python3
"""
Q-BTC live mining monitor (network-first)

A self-updating terminal view that answers one question first: "is my node mining
on the SAME chain as the network?". It compares what your node is doing with data
fetched from the network (explorer, pool API) and from your own node's RPC.

  * Local node log  -> only local facts: hashrate, GPU, backend, block being mined.
  * Network sources -> chain tip, block list, difficulty, wallet balance.
  * Verdict banner  -> green = same chain, yellow = behind / unverified, red = wrong chain.

Run it in a NEW terminal while the node keeps mining:
    python3 qbtc-monitor.py --address qbtc1...
Keys: q quit | u UTC/local | r refresh now | p pause

Read-only. Python 3.8+, standard library only. Never touches wallets, keys or seed
phrases. Network calls are small and rate-limited (about one request per source
every 60 s). Use --offline to disable them. Use --discover to print what each
endpoint returns (useful when the explorer API changes).
"""
from __future__ import annotations

import argparse
import glob
import json
import os
import random
import re
import select
import shutil
import signal
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone

VERSION = "2.0"
TWO64 = 2 ** 64
UA = "Mozilla/5.0 (compatible; qbtc-monitor/%s; read-only)" % VERSION

DEFAULT_EXPLORER = "https://explorer.qbtc-core.org"
DEFAULT_POOL = "http://144.172.110.193:3335"

RE_ANSI = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]")
RE_RATE = re.compile(r"([\d.]+)\s*([kKMGT]?)H/s")
RE_MINING_DEV = re.compile(r"\[MINING\]\s+(\S+?)\s+progress")
RE_BACKEND = re.compile(r"Miner: (\w+) backend selected")
RE_DEVICE = re.compile(r"CUDA device (\d+) ready: (.+?)\s*$")
RE_ADDR = re.compile(r"^qbtc1[0-9a-z]{20,}$")
MULT = {"": 1.0, "k": 1e3, "K": 1e3, "M": 1e6, "G": 1e9, "T": 1e12}


# ----------------------------------------------------------------------------- helpers
def parse_ts(s):
    """'2026-10-17T14:23:17.123456Z' / '2026-10-17 14:23:17' -> epoch seconds (UTC)."""
    try:
        s = str(s).strip().rstrip("Z").replace(" ", "T")
        if "." in s:
            head, frac = s.split(".", 1)
            s = head + "." + frac[:6]
        return datetime.fromisoformat(s).replace(tzinfo=timezone.utc).timestamp()
    except Exception:
        return 0.0


def fmt_rate(hps):
    for lim, unit in ((1e12, "TH/s"), (1e9, "GH/s"), (1e6, "MH/s"), (1e3, "kH/s")):
        if hps >= lim:
            return "%.2f" % (hps / lim), unit
    return "%.2f" % hps, "H/s"


def fmt_dur(sec):
    sec = int(max(0, sec))
    d, r = divmod(sec, 86400)
    h, r = divmod(r, 3600)
    m, s = divmod(r, 60)
    if d:
        return "%dd %02dh" % (d, h)
    if h:
        return "%dh %02dm" % (h, m)
    if m:
        return "%dm %02ds" % (m, s)
    return "%ds" % s


def vlen(s):
    return len(RE_ANSI.sub("", s))


def pad(s, n):
    return s + " " * max(0, n - vlen(s))


def short_hash(h):
    """PoW hashes start with many zeros, so show the informative part."""
    if not h:
        return "-"
    t = h.lstrip("0")
    return "0.." + t[:10] if t else "0"


def short_addr(a):
    return (a[:9] + ".." + a[-6:]) if a and len(a) > 20 else (a or "-")


class Style:
    def __init__(self, enabled):
        self.on = enabled

    def c(self, code, text):
        return "\033[%sm%s\033[0m" % (code, text) if self.on else text

    def green(self, t): return self.c("1;32", t)
    def red(self, t): return self.c("1;31", t)
    def yellow(self, t): return self.c("1;33", t)
    def cyan(self, t): return self.c("1;36", t)
    def dim(self, t): return self.c("2", t)
    def bold(self, t): return self.c("1", t)


BIG = {
    "0": ["█████", "█   █", "█   █", "█   █", "█████"],
    "1": ["  █  ", " ██  ", "  █  ", "  █  ", "█████"],
    "2": ["█████", "    █", "█████", "█    ", "█████"],
    "3": ["█████", "    █", "█████", "    █", "█████"],
    "4": ["█   █", "█   █", "█████", "    █", "    █"],
    "5": ["█████", "█    ", "█████", "    █", "█████"],
    "6": ["█████", "█    ", "█████", "█   █", "█████"],
    "7": ["█████", "    █", "   █ ", "  █  ", "  █  "],
    "8": ["█████", "█   █", "█████", "█   █", "█████"],
    "9": ["█████", "█   █", "█████", "    █", "█████"],
    ".": ["   ", "   ", "   ", "   ", " █ "],
    " ": ["  ", "  ", "  ", "  ", "  "],
}


def big_text(text):
    rows = ["", "", "", "", ""]
    for ch in text:
        g = BIG.get(ch, BIG[" "])
        for i in range(5):
            rows[i] += g[i] + " "
    return rows


# ----------------------------------------------------------------------------- tolerant JSON extraction
# The node/explorer JSON field names are not documented, so every reader below looks
# for several likely names and reports "unavailable" instead of guessing.
H_KEYS = ("height", "block_height", "chain_height", "tip_height", "current_height",
          "best_height", "blocks", "block_count", "index")
HASH_KEYS = ("hash", "block_hash", "blockhash", "tip_hash", "best_hash", "bestblockhash",
             "current_hash", "latest_hash")
TIME_KEYS = ("timestamp", "time", "mined_on", "mined_at", "mined", "created_at", "date", "ts")
TARGET_KEYS = ("target", "required_target", "difficulty_target", "current_target")
MINER_KEYS = ("miner", "miner_address", "coinbase_address", "reward_address", "mined_by",
              "finder", "coinbase", "address")
TX_KEYS = ("tx_count", "txs", "num_txs", "transactions", "tx")
HR_KEYS = ("network_hashrate", "hashrate", "network_hashps", "hash_rate")
CONF_KEYS = ("confirmed", "confirmed_balance", "available", "spendable", "balance")
PEND_KEYS = ("pending", "pending_mempool", "mempool", "unconfirmed", "pending_balance")
LOCK_KEYS = ("locked", "locked_coinbase", "immature", "maturing", "coinbase_locked", "locked_balance")

BALANCE_UNIT = "auto"      # auto | qbtc | base   (set from --balance-unit)


def walk(obj, depth=0):
    """Yield every dict inside a JSON value (depth-limited)."""
    if depth > 5:
        return
    if isinstance(obj, dict):
        yield obj
        for v in obj.values():
            if isinstance(v, (dict, list)):
                for d in walk(v, depth + 1):
                    yield d
    elif isinstance(obj, list):
        for v in obj[:200]:
            if isinstance(v, (dict, list)):
                for d in walk(v, depth + 1):
                    yield d


def pick(d, keys):
    low = {str(k).lower(): v for k, v in d.items()}
    for k in keys:
        if k in low and low[k] is not None:
            return low[k]
    return None


def to_int(v):
    try:
        if isinstance(v, bool):
            return None
        if isinstance(v, int):
            return v
        if isinstance(v, float) and v == int(v):
            return int(v)
        if isinstance(v, str):
            s = v.strip().replace(",", "").lstrip("#")
            if s.lower().startswith("0x"):
                return int(s, 16)
            if re.fullmatch(r"\d+", s):
                return int(s)
    except Exception:
        pass
    return None


def to_float(v):
    try:
        if isinstance(v, bool):
            return None
        if isinstance(v, (int, float)):
            return float(v)
        if isinstance(v, str):
            m = re.search(r"-?\d[\d,]*\.?\d*", v)
            if m:
                return float(m.group(0).replace(",", ""))
    except Exception:
        pass
    return None


def norm_hash(v):
    if not isinstance(v, str):
        return None
    s = v.strip().lower()
    if s.startswith("0x"):
        s = s[2:]
    return s if re.fullmatch(r"[0-9a-f]{16,}", s) else None


def to_epoch(v):
    if isinstance(v, (int, float)) and not isinstance(v, bool):
        return v / 1000.0 if v > 1e12 else float(v)
    if isinstance(v, str):
        s = v.strip()
        if re.fullmatch(r"\d{9,13}", s):
            return to_epoch(int(s))
        return parse_ts(s)
    return 0.0


def find_addr(v, depth=0):
    if isinstance(v, str):
        return v if RE_ADDR.match(v) else None
    if depth > 4:
        return None
    if isinstance(v, dict):
        for x in v.values():
            a = find_addr(x, depth + 1)
            if a:
                return a
    elif isinstance(v, list):
        for x in v[:20]:
            a = find_addr(x, depth + 1)
            if a:
                return a
    return None


CB_ZERO = set("0")


def coinbase_info(txs):
    """(miner key-hash, reward in sats) from the first coinbase transaction of a block."""
    if not isinstance(txs, list):
        return None, None
    for tx in txs:
        if not isinstance(tx, dict):
            continue
        ins = tx.get("inputs") or []
        if ins and all(isinstance(i, dict) and set(str(i.get("prev_txid", "x"))) <= CB_ZERO for i in ins):
            outs = tx.get("outputs") or []
            if outs and isinstance(outs[0], dict):
                return norm_hash(outs[0].get("address_hash")), to_int(outs[0].get("value_sats"))
    return None, None


def make_block(d):
    """The node's get_block JSON has NO 'hash' field. It has previous_hash, merkle_root, nonce,
    target, timestamp and the coinbase output (address_hash = miner's key hash)."""
    h = to_int(pick(d, H_KEYS))
    hh = norm_hash(pick(d, HASH_KEYS))
    prev = norm_hash(pick(d, ("previous_hash", "prev_hash", "parent_hash")))
    if h is None and hh is None and prev is None:
        return None
    txv = pick(d, TX_KEYS)
    txs = len(txv) if isinstance(txv, list) else to_int(txv)
    mhash, reward = coinbase_info(d.get("transactions"))
    miner = None
    for k in MINER_KEYS:
        v = pick(d, (k,))
        if v is not None:
            miner = find_addr(v)
            if miner:
                break
    return {"h": h, "hash": hh, "prev": prev, "merkle": norm_hash(pick(d, ("merkle_root",))),
            "nonce": to_int(pick(d, ("nonce",))), "ts": to_epoch(pick(d, TIME_KEYS)),
            "target": to_int(pick(d, TARGET_KEYS)), "miner": miner, "mhash": mhash,
            "reward": reward, "txs": txs}


def extract_blocks(obj):
    """A list of block-like dicts anywhere in the JSON -> sorted newest first, or None."""
    best = None
    stack = [obj]
    seen = 0
    while stack and seen < 400:
        cur = stack.pop()
        seen += 1
        if isinstance(cur, list):
            recs = [make_block(x) for x in cur if isinstance(x, dict)]
            recs = [r for r in recs if r and r["h"] is not None]
            if recs and (best is None or len(recs) > len(best)):
                best = recs
            stack.extend(x for x in cur[:50] if isinstance(x, (dict, list)))
        elif isinstance(cur, dict):
            stack.extend(v for v in cur.values() if isinstance(v, (dict, list)))
    if best:
        return sorted(best, key=lambda r: -r["h"])
    return None


def extract_info(obj):
    for d in walk(obj):
        h = to_int(pick(d, H_KEYS))
        if h is not None and h > 0:
            return {"h": h, "hash": norm_hash(pick(d, HASH_KEYS)),
                    "hashrate": to_float(pick(d, HR_KEYS)),
                    "target": to_int(pick(d, TARGET_KEYS))}
    return None


def extract_one_block(obj):
    for d in walk(obj):
        b = make_block(d)
        if b and b["h"] is not None and (b["prev"] or b["hash"] or b["nonce"] is not None):
            return b
    return None


def to_amount(v, key=""):
    if isinstance(v, bool) or v is None:
        return None
    if isinstance(v, str):
        return to_float(v)
    if isinstance(v, float):
        return v
    if isinstance(v, int):
        if BALANCE_UNIT == "qbtc":
            return float(v)
        if BALANCE_UNIT == "base" or True:      # integers from the node are base units (1e-8)
            return v / 1e8
    return None


def extract_balance(obj):
    """Verified explorer format: confirmed_sats / unconfirmed_sats / pending_sats / locked_sats
    (1 QBTC = 1e8 sats). Falls back to plain QBTC field names."""
    for d in walk(obj):
        low = {str(k).lower(): v for k, v in d.items()}
        if any(k.endswith("_sats") for k in low):
            out = {}
            for name, key in (("confirmed", "confirmed_sats"), ("pending", "pending_sats"),
                              ("locked", "locked_sats"), ("spendable", "max_send_sats"),
                              ("unconfirmed", "unconfirmed_sats")):
                v = to_int(low.get(key))
                if v is not None:
                    out[name] = v / 1e8
            if out:
                return out
        out = {}
        for name, keys in (("confirmed", CONF_KEYS), ("pending", PEND_KEYS), ("locked", LOCK_KEYS)):
            for k in keys:
                v = pick(d, (k,))
                a = to_amount(v, k) if not isinstance(v, (dict, list)) else None
                if a is not None:
                    out[name] = a
                    break
        if out:
            return out
    return None


# ----------------------------------------------------------------------------- HTTP + endpoint probing
def http(url, method="GET", body=None, headers=None, timeout=5, maxbytes=2000000):
    h = {"User-Agent": UA, "Accept": "application/json"}
    if headers:
        h.update(headers)
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        h["Content-Type"] = "application/json"
    req = urllib.request.Request(url, data=data, headers=h, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read(maxbytes)
    except urllib.error.HTTPError as e:
        try:
            return e.code, e.read(2000)
        except Exception:
            return e.code, b""
    except Exception as e:
        return 0, str(e).encode()


def http_json(url, method="GET", body=None, headers=None, timeout=5):
    st, raw = http(url, method, body, headers, timeout)
    if st != 200:
        return None
    try:
        return json.loads(raw.decode("utf-8", "replace"))
    except Exception:
        return None


# A spec is (method, path template, body key or None, body value name or None).
G = "GET"
P = "POST"
INFO_SPECS = [(G, "/api/get_info", None, None), (G, "/api/info", None, None),
              (G, "/api/stats", None, None), (G, "/api/status", None, None),
              (G, "/api/network", None, None), (G, "/api/v1/info", None, None)]
BLOCKS_SPECS = [(G, "/api/blocks", None, None), (G, "/api/latest_blocks", None, None),
                (G, "/api/blocks/latest", None, None), (G, "/api/recent_blocks", None, None),
                (G, "/api/get_blocks", None, None), (G, "/api/explorer/blocks", None, None),
                (G, "/api/blocks?limit=20", None, None), (G, "/api/latest-blocks", None, None),
                (G, "/api/v1/blocks", None, None)]
BLOCK_AT_SPECS = [(P, "/api/get_block", "height", "h"), (P, "/api/get_block", "block_height", "h"),
                  (P, "/api/get_block", "index", "h"), (G, "/api/get_block?height={h}", None, None),
                  (G, "/api/block/{h}", None, None), (G, "/api/blocks/{h}", None, None),
                  (G, "/api/get_block/{h}", None, None), (G, "/api/block?height={h}", None, None)]
BALANCE_SPECS = [(P, "/api/get_balance", "address", "a"), (P, "/api/get_balance", "target", "a"),
                 (P, "/api/get_balance", "wallet", "a"), (G, "/api/get_balance?address={a}", None, None),
                 (G, "/api/balance/{a}", None, None), (G, "/api/address/{a}", None, None),
                 (G, "/api/get_balance/{a}", None, None), (G, "/api/balance?address={a}", None, None)]
REPROBE = 300.0
NET_MIN = 30.0          # hard floor for requests to shared servers (seconds)
LAG_TOL = 120.0         # how stale network data may be before "behind/ahead" is alarming; set in main()


class Api:
    """One HTTP API whose exact routes are discovered by trying a short candidate list."""

    def __init__(self, name, base, headers=None):
        self.name = name
        self.base = base.rstrip("/")
        self.headers = headers or {}
        self.spec = {}
        self.last_probe = {}

    def call(self, spec, **kw):
        method, path, key, var = spec
        url = self.base + path.format(**kw)
        body = {key: kw[var]} if key else None
        return http_json(url, method, body, self.headers)

    def get(self, purpose, candidates, valid, probe=True, **kw):
        now = time.time()
        spec = self.spec.get(purpose)
        if spec:
            obj = self.call(spec, **kw)
            val = valid(obj) if obj is not None else None
            if val is not None:
                return val
            if not probe:
                return None
        lp = self.last_probe.get(purpose)
        if lp is not None and now - lp < REPROBE:
            return None
        self.last_probe[purpose] = now
        for sp in candidates:
            obj = self.call(sp, **kw)
            val = valid(obj) if obj is not None else None
            if val is not None:
                self.spec[purpose] = sp
                return val
        self.spec.pop(purpose, None)
        return None

    def reset(self):
        self.last_probe.clear()


# ----------------------------------------------------------------------------- network poller (background thread)
LIST_N = 15
EXPL_INFO = INFO_SPECS[:1]            # GET  /api/get_info            (verified on the explorer)
ONE_BLOCK = BLOCK_AT_SPECS[:1]        # POST /api/get_block {"height"} (verified on the explorer)
ONE_BAL = BALANCE_SPECS[:1]           # POST /api/get_balance {"address"} (verified on the explorer)


def link_hashes(blocks):
    """The API gives no block hash, but block h+1 carries previous_hash = hash of block h."""
    by_h = {b["h"]: b for b in blocks}
    out = []
    for b in blocks:
        c = dict(b)
        nxt = by_h.get(b["h"] + 1)
        c["hash"] = nxt["prev"] if nxt and nxt.get("prev") else None
        out.append(c)
    return out


class Net:
    def __init__(self, args, node_port_fn):
        self.args = args
        self.node_port_fn = node_port_fn
        self.lock = threading.Lock()
        self.stop = False
        self.kick = False
        base = args.explorer.rstrip("/") if args.explorer else ""
        self.expl = Api("explorer", base, {"Origin": base, "Referer": base + "/"}) if base else None
        self.pool_base = args.pool_api.rstrip("/") if args.pool_api else ""
        tok = os.environ.get("QBTC_RPC_TOKEN", "")
        self.rpc_token = {"Authorization": tok} if tok else {}
        self.rpc = None
        self.ecache = {}
        self.lcache = {}
        self.snap = {
            "pool": {"ok": False, "ts": 0, "next": None, "target": None, "err": ""},
            "expl": {"ok": False, "ts": 0, "blocks": [], "tip": None},
            "local": {"ok": False, "ts": 0, "tip": None, "blocks": []},
            "bal": {"ok": False, "ts": 0, "data": None, "addr": ""},
        }
        self.t = {"local": 0.0}
        self.next = {}
        self.fails = {}
        self.last_try = {}
        self.address = ""
        self.thread = threading.Thread(target=self.loop, daemon=True)

    def start(self):
        self.thread.start()

    def get(self):
        with self.lock:
            return {k: dict(v) for k, v in self.snap.items()}

    def put(self, key, **kw):
        with self.lock:
            self.snap[key].update(kw)

    def due(self, key, base, now):
        return now >= self.next.get(key, 0.0)

    def done(self, key, base, ok):
        """Schedule the next request: base interval +-20 % jitter (so miners do not all hit the
        server together); double the wait after each failure, up to 15 minutes."""
        self.fails[key] = 0 if ok else min(self.fails.get(key, 0) + 1, 4)
        wait = base * (2 ** self.fails[key]) if not ok else base
        self.next[key] = time.time() + min(wait, 900.0) * random.uniform(0.8, 1.2)

    def loop(self):
        a = self.args
        ni = a.net_interval
        while not self.stop:
            now = time.time()
            if self.kick:
                self.kick = False
                self.next["local"] = 0.0
                for k in ("pool", "expl", "bal"):          # manual refresh, but never faster than every 15 s
                    if now - self.last_try.get(k, 0.0) >= 15:
                        self.next[k] = 0.0
                for api in (self.expl, self.rpc):
                    if api:
                        api.reset()
            try:
                if a.rpc != "off" and now - self.t["local"] >= max(a.interval, 3):     # your own machine
                    self.t["local"] = now
                    self.poll_local()
                if not a.offline:
                    if self.pool_base and self.due("pool", ni, now):
                        self.last_try["pool"] = now
                        self.poll_pool()
                        self.done("pool", ni, self.snap["pool"]["ok"])
                    if self.expl and self.due("expl", ni, now):
                        self.last_try["expl"] = now
                        self.poll_explorer()
                        self.done("expl", ni, self.snap["expl"]["ok"])
                    if self.expl and self.address and self.due("bal", ni, now):
                        self.last_try["bal"] = now
                        self.poll_balance()
                        self.done("bal", max(2 * ni, 120.0), self.snap["bal"]["ok"])
            except Exception as e:                      # never let the thread die
                self.put("pool", err="internal: %s" % e)
            time.sleep(0.5)

    # -- helpers
    def fb(self, api, cache, specs, h, max_age=None, probe=False):
        """Block at height h, cached (finished blocks never change; the newest two are re-read)."""
        now = time.time()
        hit = cache.get(h)
        if hit and (max_age is None or now - hit[1] < max_age):
            return hit[0]
        b = api.get("block", specs, extract_one_block, probe=probe, h=h)
        if b is not None:
            b["h"] = h
            cache[h] = (b, now)
            if len(cache) > 300:
                for k in sorted(cache)[:100]:
                    cache.pop(k, None)
            return b
        return hit[0] if hit else None

    def chain(self, api, cache, info_specs, block_specs, n):
        """{'tip','blocks'} of one node/explorer. get_info.current_height may be the tip OR tip+1,
        so ask whether a block exists at that height."""
        info = api.get("info", info_specs, extract_info)
        if not info:
            return None
        ch = info["h"]
        if self.fb(api, cache, block_specs, ch - 1, max_age=300, probe=True) is None:
            return None
        cur = self.fb(api, cache, block_specs, ch, max_age=30)
        tip = ch if cur else ch - 1
        blocks = []
        for h in range(tip, max(tip - n, 0), -1):
            b = self.fb(api, cache, block_specs, h, max_age=(300 if h >= tip - 1 else None))
            if b:
                blocks.append(b)
        return {"tip": tip, "blocks": link_hashes(blocks)}

    # -- sources
    def poll_pool(self):
        obj = http_json(self.pool_base + "/api/stats")
        if isinstance(obj, dict) and to_int(obj.get("current_height")):
            self.put("pool", ok=True, ts=time.time(), next=to_int(obj.get("current_height")),
                     target=to_int(obj.get("current_target")), err="")
        else:
            self.put("pool", ok=False, err="no answer")

    def poll_explorer(self):
        st = self.chain(self.expl, self.ecache, EXPL_INFO, ONE_BLOCK, LIST_N)
        if st:
            self.put("expl", ok=True, ts=time.time(), blocks=st["blocks"], tip=st["tip"])
        else:
            self.put("expl", ok=False)

    def poll_local(self):
        explicit = self.args.rpc not in ("", "auto", "off")
        port = self.node_port_fn()
        if port is None and not explicit:
            self.put("local", ok=False)
            return
        base = self.args.rpc if explicit else "http://127.0.0.1:%d" % (port + 4001)
        if self.rpc is None or self.rpc.base != base.rstrip("/"):
            self.rpc = Api("node-rpc", base, self.rpc_token)
            self.lcache = {}
        st = self.chain(self.rpc, self.lcache, [(G, "/api/get_info", None, None)], BLOCK_AT_SPECS[:3], 4)
        if st:
            self.put("local", ok=True, ts=time.time(), tip=st["tip"], blocks=st["blocks"])
        else:
            self.put("local", ok=False)

    def poll_balance(self):
        bal = self.expl.get("balance", ONE_BAL, extract_balance, a=self.address)
        if bal:
            self.put("bal", ok=True, ts=time.time(), data=bal, addr=self.address)
        else:
            self.put("bal", ok=False, addr=self.address)


# ----------------------------------------------------------------------------- local node log (local facts only)
class State:
    """Replays node log lines. Used ONLY for local facts: hashrate, GPU, backend, the
    block being mined, and the hashes of blocks this node itself found."""

    def __init__(self):
        self.backend = ""
        self.devices = {}
        self.rates = {}
        self.active = False
        self.cur_height = 0
        self.cur_target = 0
        self.round_start = 0.0
        self.mining_since = 0.0
        self.last_interrupt = 0.0
        self.last_sync = 0.0
        self.tip = 0                 # last tip seen in the log (fallback when the RPC is unreachable)
        self.pend = False
        self.secured = False
        self.pend_hash = None
        self.pend_nonce = None
        self.own = []                # {"h","hash","nonce","ts"}

    def feed(self, line):
        if not line:
            return
        sp = line.find(" ")
        ts = parse_ts(line[:sp]) if sp > 0 else 0.0
        if "[MINING]" in line:
            m = RE_RATE.search(line)
            d = RE_MINING_DEV.search(line)
            if m and d:
                self.rates[d.group(1)] = (float(m.group(1)) * MULT[m.group(2)], ts)
            return
        if "million hashes calculated" in line:
            return
        if "backend selected" in line:
            m = RE_BACKEND.search(line)
            if m:
                self.backend = {"CUDA": "GPU (CUDA)", "CPU": "CPU", "Hybrid": "CPU + GPU"}.get(m.group(1), m.group(1))
        elif "CUDA device" in line and "ready:" in line:
            m = RE_DEVICE.search(line)
            if m:
                self.devices[int(m.group(1))] = m.group(2)
        elif "Starting Proof-of-Work for block height" in line:
            try:
                self.cur_height = int(line.rsplit("block height", 1)[1].strip(" ."))
            except ValueError:
                pass
            if not self.mining_since or ts - self.last_interrupt > 15:
                self.mining_since = ts
            self.active = True
            self.round_start = ts
        elif "Target threshold set to" in line:
            try:
                self.cur_target = int(line.rsplit("set to", 1)[1].strip(" ."))
            except ValueError:
                pass
        elif "Interrupt signal received" in line:
            self.last_interrupt = ts
            self.active = False
            self.pend = self.secured = False
        elif "Absolute Block Hash:" in line:
            self.pend_hash = norm_hash(line.rsplit("Hash:", 1)[1].strip())
        elif "puzzle solved" in line:
            m = re.search(r"nonce=(\d+)", line)
            if m:
                self.pend_nonce = int(m.group(1))
            if self.active:
                self.pend = True
        elif "Local block physically secured" in line:
            if self.active:
                self.pend = self.secured = True
        elif "Block validated. Chain height: " in line:
            try:
                h = int(re.match(r"\d+", line.rsplit("Chain height: ", 1)[1]).group(0))
            except Exception:
                return
            if self.active and self.pend and self.secured:
                self.own.append({"h": h, "hash": self.pend_hash, "nonce": self.pend_nonce, "ts": ts})
                self.own = self.own[-200:]
            self.tip = max(self.tip, h)
            self.pend = self.secured = False
            self.pend_hash = None
            self.pend_nonce = None
        elif "Nakamoto Reorg successful" in line and "New tip height: " in line:
            try:
                self.tip = int(re.match(r"\d+", line.rsplit("New tip height: ", 1)[1]).group(0))
            except Exception:
                pass
        elif "Sync: Received" in line:
            self.last_sync = ts

    def total_rate(self, now, max_age=20.0):
        live = [r for r, t in self.rates.values() if now - t <= max_age]
        return sum(live), len(live)


class LogReader:
    def __init__(self, logdir, days):
        self.logdir = logdir
        self.days = days
        self.state = State()
        self.cur = None
        self.pos = 0

    def files(self):
        return sorted(glob.glob(os.path.join(self.logdir, "qbtc_node.log.*")))

    def load(self):
        self.state = State()
        files = self.files()
        if not files:
            return False
        if self.days > 0:
            files = files[-self.days:]
        for f in files:
            try:
                with open(f, "r", errors="replace") as fh:
                    for line in fh:
                        self.state.feed(line.rstrip("\n"))
            except OSError:
                pass
        self.cur = files[-1]
        self.pos = os.path.getsize(self.cur)
        return True

    def poll(self):
        files = self.files()
        if not files:
            return
        if self.cur is None:
            self.cur, self.pos = files[-1], 0
        try:
            size = os.path.getsize(self.cur)
        except OSError:
            return
        if size < self.pos:
            self.pos = 0
        if size > self.pos:
            with open(self.cur, "r", errors="replace") as fh:
                fh.seek(self.pos)
                data = fh.read()
                cut = data.rfind("\n")
                if cut >= 0:
                    for line in data[:cut].split("\n"):
                        self.state.feed(line)
                    self.pos += len(data[:cut + 1].encode("utf-8", "replace"))
        if files[-1] != self.cur:
            self.cur, self.pos = files[-1], 0


def find_logdir(explicit):
    cands = []
    if explicit:
        cands.append(explicit)
    if os.environ.get("QBTC_LOGDIR"):
        cands.append(os.environ["QBTC_LOGDIR"])
    try:
        for pid in os.listdir("/proc"):
            if pid.isdigit():
                try:
                    with open("/proc/%s/comm" % pid) as fh:
                        if fh.read().strip() == "quantum-btc":
                            cands.append(os.path.join(os.readlink("/proc/%s/cwd" % pid), "logs"))
                except OSError:
                    continue
    except OSError:
        pass
    home = os.path.expanduser("~")
    cands += [os.path.join(home, "quantum-btc", "logs"), "/workspace/quantum-btc/logs",
              os.path.join(os.getcwd(), "logs")]
    for c in cands:
        if glob.glob(os.path.join(c, "qbtc_node.log.*")):
            return c
    return None


# ----------------------------------------------------------------------------- extras: GPU, node process, wallet address
BECH = "qpzry9x8gf2tvdw0s3jn54khce6mua7l"


def addr_hashes(addr):
    """Candidate 32-byte key hashes of a qbtc1... address (bech32 payload), to recognise our
    coinbase output (address_hash) in the block data. Checksum is not verified here."""
    out = set()
    if not RE_ADDR.match(addr or ""):
        return out
    data = [BECH.find(c) for c in addr[5:-6]]
    if not data or min(data) < 0:
        return out

    def conv(vals):
        acc = bits = 0
        res = bytearray()
        for v in vals:
            acc = (acc << 5) | v
            bits += 5
            while bits >= 8:
                bits -= 8
                res.append((acc >> bits) & 255)
            acc &= (1 << bits) - 1
        return bytes(res)
    for vals in (data, data[1:]):
        b = conv(vals)
        if len(b) >= 32:
            out.add(b[:32].hex())
    return out


def run(cmd, timeout=3):
    try:
        return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout).stdout
    except Exception:
        return ""


class Extras:
    def __init__(self, args):
        self.args = args
        self.gpus = []
        self.node_pid = ""
        self.node_port = None
        self.target = args.wallet or ""        # what auto_mine was started with (name or address)
        self.address = args.address or os.environ.get("QBTC_ADDRESS", "")
        self._t_gpu = self._t_pid = self._t_tmux = 0.0
        self.user_hashes = set()
        self._ah = ""

    def refresh(self, now):
        if now - self._t_gpu > 10 and shutil.which("nvidia-smi"):
            self._t_gpu = now
            out = run(["nvidia-smi", "--query-gpu=index,name,utilization.gpu,temperature.gpu,power.draw",
                       "--format=csv,noheader,nounits"])
            gpus = []
            for ln in out.strip().splitlines():
                p = [x.strip() for x in ln.split(",")]
                if len(p) >= 5:
                    gpus.append({"idx": p[0], "name": p[1], "util": p[2], "temp": p[3], "power": p[4]})
            self.gpus = gpus
        if now - self._t_pid > 10:
            self._t_pid = now
            out = run(["pgrep", "-x", "quantum-btc"], 2).split()
            self.node_pid = out[0] if out else ""
            self.node_port = None
            if self.node_pid:
                try:
                    with open("/proc/%s/cmdline" % self.node_pid, "rb") as fh:
                        parts = fh.read().decode("utf-8", "replace").split("\0")
                    self.node_port = 8000
                    for i, p in enumerate(parts):
                        if p == "--port" and i + 1 < len(parts) and parts[i + 1].isdigit():
                            self.node_port = int(parts[i + 1])
                except OSError:
                    self.node_port = 8000
        if not self.target and now - self._t_tmux > 60 and shutil.which("tmux"):
            self._t_tmux = now
            # The node prints "Target locked: X" on its console only (not in the log file).
            for s in run(["tmux", "list-sessions", "-F", "#{session_name}"], 2).split():
                if s.startswith("qbtc"):
                    scr = run(["tmux", "capture-pane", "-p", "-S", "-20000", "-t", s + ":0"], 3)
                    hits = re.findall(r"Target locked: (\S+)", scr)
                    if hits:
                        self.target = hits[-1]
                        break
        if not self.address and RE_ADDR.match(self.target or ""):
            self.address = self.target
        if self.address != self._ah:
            self._ah = self.address
            self.user_hashes = addr_hashes(self.address)


# ----------------------------------------------------------------------------- verdict: are we on the network's chain?
def same_hash(a, b):
    if not a or not b:
        return None
    n = min(len(a), len(b))
    if n < 16:
        return None
    return a[:n] == b[:n]


class Memory:
    """Remembers since when a mismatch has lasted, so short normal lags do not alarm."""

    def __init__(self):
        self.lag_key = None
        self.lag_since = 0.0

    def lag_age(self, key, now):
        if key is None:
            self.lag_key = None
            return 0.0
        if self.lag_key != key:
            self.lag_key, self.lag_since = key, now
        return now - self.lag_since


def block_tag(b):
    if b is None:
        return "-"
    if b.get("nonce") is not None:
        return "nonce %d" % b["nonce"]
    return short_hash(b.get("hash"))


def same_block(a, b):
    """True / False / None(unknown). Blocks are compared by parent, merkle root and nonce."""
    res = None
    for k in ("prev", "merkle", "nonce"):
        x, y = a.get(k), b.get(k)
        if x is None or y is None:
            continue
        if x != y:
            return False
        res = True
    if a.get("hash") and b.get("hash"):
        eq = same_hash(a["hash"], b["hash"])
        if eq is False:
            return False
        if eq:
            res = True
    return res


def network_view(snap):
    """(tip height, next height, source text)"""
    ex, pool = snap["expl"], snap["pool"]
    if ex["ok"] and ex["tip"]:
        return ex["tip"], ex["tip"] + 1, "explorer"
    if pool["ok"] and pool["next"]:
        return pool["next"] - 1, pool["next"], "pool API"
    return None, None, ""


def judge(snap, st, mem, now):
    """Return dict(level, title, lines, own). level: ok | warn | bad | unknown."""
    ntip, nnext, src = network_view(snap)
    local, pool = snap["local"], snap["pool"]
    out = {"level": "unknown", "title": "NETWORK DATA UNAVAILABLE", "lines": [], "ntip": ntip,
           "nnext": nnext, "src": src, "own": []}
    if ntip is None:
        out["lines"] = ["Cannot reach the explorer or pool API, so your chain could NOT be verified.",
                        "Hashing continues, but do not trust the numbers below. Check this machine's internet."]
        mem.lag_age(None, now)
        return out

    local_tip = local["tip"] if local["ok"] else (st.tip or None)
    mining = st.cur_height if (st.active and st.cur_height) else None
    ref = mining or (local_tip + 1 if local_tip else None)
    nb_by_h = {b["h"]: b for b in snap["expl"]["blocks"]}
    min_h = min(nb_by_h) if nb_by_h else None

    # own blocks (found by this miner) vs the network chain
    for o in st.own[-12:]:
        nb = nb_by_h.get(o["h"])
        if nb is None and min_h is not None and o["h"] < min_h:
            continue
        if nb is not None:
            if o.get("nonce") is not None and nb.get("nonce") is not None:
                eq = (o["nonce"] == nb["nonce"])
            else:
                eq = same_hash(o.get("hash"), nb.get("hash"))
            out["own"].append((o, "ok" if eq else ("lost" if eq is False else "wait")))
        elif o["h"] > ntip and now - o["ts"] > 3 * LAG_TOL:
            out["own"].append((o, "lost"))
        else:
            out["own"].append((o, "wait"))

    # 1) same height, different block -> different chain (strongest evidence)
    bad, good = [], 0
    for lb in local["blocks"] if local["ok"] else []:
        nb = nb_by_h.get(lb["h"])
        if not nb:
            continue
        eq = same_block(lb, nb)
        if eq is True:
            good += 1
        elif eq is False:
            bad.append((lb["h"], lb, nb))
    if bad:
        bad.sort(key=lambda x: x[0], reverse=True)
        h, lb, nb = bad[0]
        why = "different parent block" if (lb.get("prev") and nb.get("prev") and lb["prev"] != nb["prev"]) else "different block contents"
        out.update(level="bad", title="WRONG CHAIN - YOUR NODE DISAGREES WITH THE NETWORK",
                   lines=["Block #%d: your node has %s, the network has %s (%s)." % (h, block_tag(lb), block_tag(nb), why),
                          "Everything you mine now will be thrown away. Stop mining, then re-sync (see README/Telegram)."])
        mem.lag_age(None, now)
        return out

    # 2) same next block, different difficulty -> different chain rules/history
    pt = pool["target"] if pool["ok"] else None
    if mining and pool["ok"] and pool["next"] == mining and pt and st.cur_target and pt != st.cur_target:
        out.update(level="bad", title="DIFFICULTY DIFFERS FROM THE NETWORK - LIKELY WRONG CHAIN",
                   lines=["Block #%d: your target is %d, the pool/network target is %d." % (mining, st.cur_target, pt),
                          "Same height with a different target means a different chain history."])
        mem.lag_age(None, now)
        return out

    # 2b) your node's own chain tip far from the network tip (node still syncing / on its own chain)
    if local["ok"] and local["tip"]:
        d = local["tip"] - ntip
        if d <= -2 or d >= 2:
            age = mem.lag_age(("node", d), now)
            if d < 0:
                lvl = "bad" if (-d >= 3 and age > LAG_TOL) else "warn"
                out.update(level=lvl, title="YOUR NODE IS %d BLOCKS BEHIND THE NETWORK" % -d,
                           lines=["Your node is at #%d, the network at #%d. It is still syncing or has lost its peers." % (local["tip"], ntip),
                                  "Mining now builds on an old block. Wait for sync to finish (or restart the node)."])
            else:
                out.update(level="bad", title="YOUR NODE IS %d BLOCKS AHEAD OF THE NETWORK - OWN CHAIN" % d,
                           lines=["Your node is at #%d, the network at #%d." % (local["tip"], ntip),
                                  "Blocks on this chain will be rejected. Stop mining and re-sync."])
            return out

    # 3) height comparison
    if ref is None:
        out.update(level="warn", title="NODE NOT MINING / NO LOCAL DATA",
                   lines=["Network tip is #%d. Your node has not reported a block height yet "
                          "(node stopped, still syncing, or auto_mine not started)." % ntip])
        mem.lag_age(None, now)
        return out

    diff = ref - nnext
    if diff == 0:
        mem.lag_age(None, now)
        if good:
            out.update(level="ok", title="ON THE NETWORK CHAIN",
                       lines=["Your node and the network have identical newest blocks (verified).",
                              "Mining block #%d, the next block of the real chain." % ref])
        else:
            out.update(level="ok", title="IN SYNC WITH THE NETWORK (height only)",
                       lines=["Mining block #%d = next network block. Block contents could not be compared "
                              "(your node's RPC is not answering)." % ref])
    elif diff < 0:
        k = -diff
        age = mem.lag_age(("behind", k), now)
        if k == 1 and age < LAG_TOL:
            out.update(level="ok", title="NEW BLOCK ARRIVING",
                       lines=["Network moved to #%d, your miner is switching to it." % ntip])
        elif k >= 3 or age > 3 * LAG_TOL:
            out.update(level="bad", title="STALE - MINING AN OLD BLOCK (%d behind)" % k,
                       lines=["Network is at #%d but your miner works on #%d for %s." % (ntip, ref, fmt_dur(age)),
                              "Your hashpower is wasted. Restart the node (auto_mine stop, Ctrl+C, qbtc start)."])
        else:
            out.update(level="warn", title="BEHIND THE NETWORK BY %d BLOCK%s" % (k, "S" if k > 1 else ""),
                       lines=["Network is at #%d, your miner works on #%d for %s." % (ntip, ref, fmt_dur(age))])
    else:
        k = diff
        age = mem.lag_age(("ahead", k), now)
        if k == 1 and age < LAG_TOL:
            out.update(level="ok", title="YOUR BLOCK IS BEING SHARED",
                       lines=["You are one block ahead; the network should show it within a minute."])
        else:
            out.update(level="bad", title="AHEAD OF THE NETWORK - YOU ARE ON YOUR OWN CHAIN",
                       lines=["Network next block is #%d but you are mining #%d." % (nnext, ref),
                              "Blocks on this chain will be rejected. Stop mining and re-sync."])
    lost = [o for o, s_ in out["own"] if s_ == "lost"]
    if lost and out["level"] == "ok":
        out["lines"].append("Note: your block #%d is not in the network chain (orphaned)." % lost[-1]["h"])
    return out


# ----------------------------------------------------------------------------- rendering
def banner(sty, v, W):
    if not sty.on:
        return ["=" * W, ("[%s] %s" % (v["level"].upper(), v["title"])).center(W), "=" * W]
    code = {"ok": "1;97;42", "warn": "1;30;43", "bad": "1;97;41", "unknown": "1;97;100"}[v["level"]]
    icon = {"ok": "✔", "warn": "!", "bad": "✖", "unknown": "?"}[v["level"]]
    t = " %s  %s  %s " % (icon, v["title"], icon)
    return [sty.c(code, " " * W), sty.c(code, t.center(W)), sty.c(code, " " * W)]


def mark(sty, state):
    return {"ok": sty.green("✓"), "bad": sty.red("✗"), "warn": sty.yellow("?"), None: sty.dim("-")}[state]


def render(st, ex, net_snap, verdict, args, sty, width, height, now, tz, paused=False):
    W = max(70, min(width, 118))
    L = []
    rule = sty.dim("─" * W)

    def fts(epoch, fmt="%Y-%m-%d %H:%M:%S"):
        return datetime.fromtimestamp(epoch, tz).strftime(fmt) if epoch else "-"

    zone = "UTC" if args.utc else (args.tz or datetime.now().astimezone().tzname() or "local")
    title = sty.bold("Q-BTC MINING MONITOR") + sty.dim("  v" + VERSION)
    right = "%s %s" % (fts(now, "%H:%M:%S"), zone)
    L.append(" " + title + " " * max(1, W - 2 - vlen(title) - len(right)) + sty.dim(right))

    # 1) THE VERDICT (most important)
    L.extend(banner(sty, verdict, W))
    for ln in verdict["lines"][:3]:
        L.append("  " + ln)
    L.append("")

    # 2) side-by-side comparison
    local, pool, expl = net_snap["local"], net_snap["pool"], net_snap["expl"]
    ntip, nnext = verdict["ntip"], verdict["nnext"]
    cw = max(24, (W - 22) // 2)
    L.append("  " + sty.bold(pad("", 15)) + sty.bold(pad("NETWORK (%s)" % (verdict["src"] or "n/a"), cw))
             + sty.bold(pad("YOUR NODE", cw)) + "")
    nb = expl["blocks"][0] if expl["blocks"] else None
    lb = local["blocks"][0] if (local["ok"] and local["blocks"]) else None
    ntxt = "-"
    if ntip:
        ntxt = "#%d" % ntip
        if nb:
            ntxt += "  " + block_tag(nb)
        if nb and nb["ts"]:
            ntxt += "  %s ago" % fmt_dur(now - nb["ts"])
    ltip = local["tip"] if local["ok"] else (st.tip or None)
    ltxt = "-"
    lstate = None
    if ltip:
        ltxt = "#%d" % ltip
        if lb:
            ltxt += "  " + block_tag(lb)
        if not local["ok"]:
            ltxt += "  (from log)"
        if ntip:
            nbh = {b["h"]: b for b in expl["blocks"]}.get(ltip)
            eq = same_block(lb, nbh) if (lb and nbh) else None
            if ltip == ntip and eq is True:
                lstate = "ok"
            elif eq is False:
                lstate = "bad"
            elif ltip == ntip:
                lstate = "warn"
            else:
                lstate = "bad" if abs(ltip - ntip) >= 2 else "warn"
    L.append("  " + pad("Latest block", 15) + pad(ntxt, cw) + pad(ltxt, cw) + mark(sty, lstate))
    # row: mining block
    mining = st.cur_height if (st.active and st.cur_height) else None
    mtxt = ("#%d  (working %s)" % (mining, fmt_dur(now - st.round_start))) if mining else sty.dim("not mining")
    mstate = None
    if mining and nnext:
        mstate = "ok" if mining == nnext else ("bad" if abs(mining - nnext) >= 2 else "warn")
    L.append("  " + pad("Mining block", 15) + pad("#%d" % nnext if nnext else "-", cw) + pad(mtxt, cw) + mark(sty, mstate))
    # row: difficulty
    pt = pool["target"] if pool["ok"] else None
    tstate = None
    if pt and st.cur_target and mining and pool["next"] == mining:
        tstate = "ok" if pt == st.cur_target else "bad"
    L.append("  " + pad("Difficulty", 15) + pad(("target %d" % pt) if pt else "-", cw)
             + pad(("target %d" % st.cur_target) if st.cur_target else "-", cw) + mark(sty, tstate))
    L.append(rule)

    # 3) hashrate + wallet
    total, ndev = st.total_rate(now)
    fresh = ndev > 0
    num, unit = fmt_rate(total if fresh else 0.0)
    colour = sty.green if fresh else sty.dim
    if height >= 40:
        for i, r in enumerate(big_text(num)):
            L.append("   " + colour(r) + (("  " + sty.bold(unit)) if i == 4 else ""))
    else:
        L.append("   " + sty.bold(colour("HASHRATE  %s %s" % (num, unit))))
    if fresh:
        parts = []
        for label in sorted(st.rates):
            rate, t = st.rates[label]
            if now - t <= 20:
                n, u = fmt_rate(rate)
                g = ""
                m = re.search(r"(\d+)$", label)
                if m:
                    for gi in ex.gpus:
                        if gi["idx"] == m.group(1):
                            g = "  %s°C %s%%" % (gi["temp"], gi["util"])
                parts.append("%s %s %s%s" % (label, n, u, g))
        L.append("   " + sty.dim("   ".join(parts)))
    else:
        L.append("   " + sty.dim("No live [MINING] lines in the last 20 s - miner idle, syncing or stopped."))
    L.append(rule)

    bal, addr = net_snap["bal"], ex.address
    if addr:
        head = "  " + sty.bold("WALLET BALANCE") + sty.dim("  (network, %s)" % short_addr(addr))
        if bal["ok"] and bal["data"]:
            d = bal["data"]
            parts = ["Confirmed " + sty.bold(sty.green("%.8f QBTC" % d.get("confirmed", 0.0)))]
            if "pending" in d:
                parts.append("Pending %.8f" % d["pending"])
            if "locked" in d:
                parts.append("Locked (coinbase) %.8f" % d["locked"])
            L.append(head)
            L.append("  " + "    ".join(parts) + sty.dim("   updated %s ago" % fmt_dur(now - bal["ts"])))
        elif args.offline:
            L.append(head + sty.dim("   offline mode"))
        else:
            L.append(head)
            L.append("  " + sty.yellow("Balance not available yet") +
                     sty.dim(" - explorer balance route not found. Run --discover and share the output."))
    else:
        L.append("  " + sty.bold("WALLET BALANCE") + sty.dim("   not set. Start with --address qbtc1... "
                                                             "(or export QBTC_ADDRESS) to see it."))
    L.append(rule)

    # 4) secondary information (small)
    if fresh:
        chip = sty.green("MINING")
    elif (now - st.last_sync < 20):
        chip = sty.yellow("SYNCING")
    elif st.active:
        chip = sty.yellow("ACTIVE (no hashrate lines yet)")
    else:
        chip = sty.red("IDLE")
    backend = st.backend or "unknown"
    if "GPU" in backend and st.devices:
        backend += " x%d" % len(st.devices)
    node = ("RUNNING (process id %s)" % ex.node_pid) if ex.node_pid else sty.red("NOT RUNNING (process not found)")
    L.append(sty.dim("  Miner: ") + chip + sty.dim("   Backend: ") + backend + sty.dim("   Node: ") + node)
    if ex.target:
        L.append(sty.dim("  Mining to: ") + ex.target)
    nh = None
    avg = None
    blocks = expl["blocks"]
    if len(blocks) >= 4 and all(b["ts"] for b in blocks[:8]):
        use = blocks[:min(len(blocks), 30)]
        dt = use[0]["ts"] - use[-1]["ts"]
        work = sum(TWO64 / b["target"] for b in use[:-1] if b["target"])
        if dt > 0 and work > 0:
            nh = work / dt
            avg = dt / (len(use) - 1)
    if nh:
        n1, u1 = fmt_rate(nh)
        line = "  Network: ~%s %s" % (n1, u1)
        if avg:
            line += " (avg block time %s)" % fmt_dur(avg)
        tg = pt or (blocks[0]["target"] if blocks else None)
        if fresh and tg:
            exp = 3600.0 * total / (TWO64 / tg)
            line += "   You ~%.1f%% of network, ~%.2f blocks/hour" % (100.0 * total / nh, exp)
        L.append(sty.dim(line))
    if nb and nb["ts"] and now - nb["ts"] > 900:
        L.append(sty.yellow("  No new block on the network for %s." % fmt_dur(now - nb["ts"])))

    def age(snap_ts):
        return fmt_dur(now - snap_ts) + " ago" if snap_ts else "never"
    src = []
    for name, ok, ts in (("explorer", expl["ok"], expl["ts"]), ("pool API", pool["ok"], pool["ts"]),
                         ("node RPC", local["ok"], local["ts"])):
        src.append((sty.green("●") if ok else sty.red("○")) + " %s %s" % (name, age(ts) if ok else "n/a"))
    L.append(sty.dim("  Data: ") + "   ".join(src) + sty.dim("   (log used only for hashrate/GPU/mining block)"))
    L.append(rule)

    # 5) recent blocks from the NETWORK
    L.append("  " + sty.bold("RECENT NETWORK BLOCKS") + "   " + sty.green("■ you") + "  " + sty.red("■ others")
             + "  " + sty.yellow("■ yours, NOT in network chain") + "  " + sty.dim("■ miner unknown"))
    L.append(sty.dim("  %-9s %-20s %-16s %-17s %s" % ("Block", "Time", "Hash", "Miner", "Note")))
    room = max(4, height - len(L) - 2)
    own_by_h = {o["h"]: s_ for o, s_ in verdict["own"]}
    uh = ex.user_hashes

    def who(m):
        return (m[:8] + ".." + m[-4:]) if m and len(m) > 14 else (m or "?")
    rows = []
    for b in blocks:
        ms = own_by_h.get(b["h"])
        mine = bool(b["mhash"] and uh and b["mhash"] in uh)
        rew = (" +%g QBTC" % (b["reward"] / 1e8)) if b.get("reward") else ""
        hid = short_hash(b["hash"]) if b["hash"] else "newest"
        if ms == "ok" or mine:
            rows.append((b["h"], b["ts"], hid, "YOU", "YOU - in the network chain" + rew, sty.green))
        elif ms == "lost":
            rows.append((b["h"], b["ts"], hid, who(b["mhash"]), "your block lost to this one", sty.yellow))
        elif b["mhash"]:
            rows.append((b["h"], b["ts"], hid, who(b["mhash"]), "", sty.red))
        else:
            rows.append((b["h"], b["ts"], hid, "?", "", lambda t: t))
    shown_h = set(b["h"] for b in blocks)
    for o, s_ in verdict["own"]:
        if o["h"] not in shown_h and s_ == "lost":
            rows.append((o["h"], o["ts"], short_hash(o["hash"]), "YOU", "NOT in network chain (orphaned)", sty.yellow))
        elif o["h"] not in shown_h and s_ == "wait":
            rows.append((o["h"], o["ts"], short_hash(o["hash"]), "YOU", "waiting for network to show it", sty.yellow))
    rows.sort(key=lambda r: -r[0])
    for h, ts, hid, miner, note, f in rows[:room]:
        L.append(f("  %-9s %-20s %-16s %-17s %s" % ("#%d" % h, fts(ts), hid, miner, note)))
    if not rows:
        L.append(sty.dim("  No network block data (explorer not answering). Run --discover."))
    return L


def build_json(st, ex, snap, verdict, now):
    total, ndev = st.total_rate(now)
    return {
        "verdict": {"level": verdict["level"], "title": verdict["title"], "lines": verdict["lines"]},
        "network": {"tip": verdict["ntip"], "next": verdict["nnext"], "source": verdict["src"]},
        "local": {"tip": snap["local"]["tip"], "rpc_ok": snap["local"]["ok"], "mining_block": st.cur_height,
                  "target": st.cur_target, "backend": st.backend, "mining_active": st.active,
                  "hashrate_hps": total, "devices": ndev, "node_pid": ex.node_pid},
        "wallet": {"target": ex.target, "address": ex.address, "balance": snap["bal"]["data"]},
        "own_blocks": [{"height": o["h"], "state": s} for o, s in verdict["own"]],
        "recent_network_blocks": [{"height": b["h"], "hash": b["hash"], "nonce": b["nonce"], "time": b["ts"], "miner_hash": b["mhash"]}
                                  for b in snap["expl"]["blocks"][:20]],
    }


# ----------------------------------------------------------------------------- --discover
def discover(args, ex):
    print("Q-BTC monitor %s - endpoint discovery (read-only). Paste this whole output when asking for help.\n" % VERSION)

    def show(title, url, method="GET", body=None, headers=None):
        st, raw = http(url, method, body, headers, 6, 400000)
        txt = raw.decode("utf-8", "replace").replace("\n", " ")
        print("%-5s %s  [%s]%s" % (method, url, st, ("  body=" + json.dumps(body)) if body else ""))
        print("      " + (txt[:300] + (" ..." if len(txt) > 300 else "")))
        return st, raw

    ex_base = args.explorer.rstrip("/") if args.explorer else ""
    if ex_base:
        print("== Explorer page: API paths found inside its HTML/JavaScript ==")
        st, raw = http(ex_base + "/", "GET", None, {"Accept": "text/html"}, 8, 1500000)
        html = raw.decode("utf-8", "replace") if st == 200 else ""
        blobs = [html]
        for src in re.findall(r"<script[^>]+src=[\"']([^\"']+)", html)[:8]:
            url = src if src.startswith("http") else ex_base + "/" + src.lstrip("/")
            if not url.startswith(ex_base):
                continue
            s2, r2 = http(url, "GET", None, {"Accept": "*/*"}, 8, 1500000)
            if s2 == 200:
                blobs.append(r2.decode("utf-8", "replace"))
        found = set()
        for b in blobs:
            for m in re.findall(r"[\"'`]((?:https?://[^\"'`\s]+)?/(?:api|rpc|v1)/[A-Za-z0-9_\-/.?=&{}$:]*)", b):
                found.add(m)
            for m in re.findall(r"fetch\(\s*[\"'`]([^\"'`]+)", b):
                found.add(m)
            for m in re.findall(r"(?:WebSocket|EventSource)\(\s*[\"'`]([^\"'`]+)", b):
                found.add(m)
        for f in sorted(found)[:60]:
            print("   ", f)
        if not found:
            print("    (none found; open the explorer in a browser, press F12 > Network > reload, and note the /api/... calls)")
        print("\n== Explorer: candidate routes ==")
        for sp in INFO_SPECS + BLOCKS_SPECS:
            show("x", ex_base + sp[1])
        if args.address:
            for sp in BALANCE_SPECS:
                m, path, key, var = sp
                show("x", ex_base + path.format(a=args.address), m, {key: args.address} if key else None)
    if args.address:
        print("\n== Address -> key-hash candidates (used to recognise your coinbase outputs) ==")
        for hx in sorted(addr_hashes(args.address)):
            print("   ", hx)
    if args.pool_api:
        print("\n== Pool API ==")
        show("x", args.pool_api.rstrip("/") + "/api/stats")
    node_port = ex.node_port or 8000
    base = args.rpc if args.rpc not in ("", "auto", "off") else "http://127.0.0.1:%d" % (node_port + 4001)
    if args.rpc != "off":
        print("\n== Local node RPC (%s) ==" % base)
        hdr = {"Authorization": os.environ["QBTC_RPC_TOKEN"]} if os.environ.get("QBTC_RPC_TOKEN") else None
        show("x", base + "/api/get_info", "GET", None, hdr)
        for sp in BLOCK_AT_SPECS:
            m, path, key, var = sp
            show("x", base + path.format(h=1), m, {key: 1} if key else None, hdr)
    print("\nDone. (No wallet, key or password was read or sent.)")


# ----------------------------------------------------------------------------- main
def main():
    global BALANCE_UNIT
    ap = argparse.ArgumentParser(description="Live Q-BTC mining monitor (network-first, read-only).")
    ap.add_argument("--address", default="", help="your wallet address (qbtc1...) to show the network balance")
    ap.add_argument("--wallet", default="", help="wallet name shown as 'Mining to' (auto-detected from tmux if possible)")
    ap.add_argument("--explorer", default=DEFAULT_EXPLORER, help="explorer base URL (default %(default)s)")
    ap.add_argument("--pool-api", default=DEFAULT_POOL, help="pool stats API base URL (default %(default)s; '' disables)")
    ap.add_argument("--rpc", default="auto", help="local node RPC base URL, 'auto' (port+4001) or 'off'")
    ap.add_argument("--offline", action="store_true", help="no explorer/pool requests (log + local node only)")
    ap.add_argument("--logdir", help="node log directory (auto-detected by default)")
    ap.add_argument("--tz", default="", help="time zone name, e.g. Asia/Kolkata")
    ap.add_argument("--utc", action="store_true", help="show UTC times")
    ap.add_argument("--days", type=int, default=1, help="daily log files to replay at start (default 1; 0 = all)")
    ap.add_argument("--interval", type=float, default=5.0,
                    help="screen/local-node refresh seconds (default 5, minimum 1; no internet traffic)")
    ap.add_argument("--net-interval", type=float, default=60.0,
                    help="seconds between requests to the shared explorer/pool (default 60, minimum 30)")
    ap.add_argument("--balance-unit", choices=("auto", "qbtc", "base"), default="auto",
                    help="how to read integer balances from the API (auto = base units of 1e-8)")
    ap.add_argument("--once", action="store_true", help="print one frame and exit")
    ap.add_argument("--json", action="store_true", help="print one JSON snapshot and exit")
    ap.add_argument("--discover", action="store_true", help="print what each endpoint returns, then exit")
    ap.add_argument("--no-color", action="store_true", help="disable colours")
    args = ap.parse_args()
    args.interval = max(1.0, args.interval)
    args.net_interval = max(NET_MIN, args.net_interval)
    global LAG_TOL
    LAG_TOL = max(90.0, 2 * args.net_interval)
    BALANCE_UNIT = args.balance_unit
    if args.address and not RE_ADDR.match(args.address):
        sys.exit("--address must look like qbtc1... (a wallet NAME is not an address)")

    try:
        os.nice(19)               # never compete with the miner
    except (OSError, AttributeError):
        pass

    tz = timezone.utc if args.utc else None
    if args.tz:
        try:
            from zoneinfo import ZoneInfo
            tz = ZoneInfo(args.tz)
        except Exception:
            sys.exit("Unknown time zone: %s" % args.tz)
    if tz is None:
        tz = datetime.now().astimezone().tzinfo

    ex = Extras(args)
    ex.refresh(time.time())
    if args.discover:
        discover(args, ex)
        return

    logdir = find_logdir(args.logdir)
    reader = LogReader(logdir or ".", args.days)
    if logdir:
        if sys.stdout.isatty() and not args.once and not args.json:
            sys.stdout.write("Loading logs from %s ...\n" % logdir)
            sys.stdout.flush()
        reader.load()
    else:
        sys.stderr.write("Note: no node log found (start the node, or pass --logdir). "
                         "Local mining details will be missing.\n")

    net = Net(args, lambda: ex.node_port)
    net.address = ex.address
    net.start()
    use_color = (sys.stdout.isatty() or os.environ.get("FORCE_COLOR")) and not args.no_color
    mem = Memory()

    def frame(sty, size, now, paused=False):
        reader.poll() if logdir else None
        ex.refresh(now)
        net.address = ex.address
        snap = net.get()
        v = judge(snap, reader.state, mem, now)
        return snap, v, render(reader.state, ex, snap, v, args, sty, size.columns, size.lines, now, tz, paused)

    if args.json or args.once or not sys.stdout.isatty():
        if not args.offline:                       # give the poller a moment for one full round
            deadline = time.time() + 12
            while time.time() < deadline:
                s = net.get()
                done = (((not net.expl or s["expl"]["ok"]) and (not net.pool_base or s["pool"]["ok"])) or (not net.expl and not net.pool_base and s["rpc"]["ok"]))
                if done and (not ex.address or s["bal"]["ok"] or time.time() > deadline - 6):
                    break
                time.sleep(0.3)
        now = time.time()
        if args.json:
            snap = net.get()
            v = judge(snap, reader.state, mem, now)
            print(json.dumps(build_json(reader.state, ex, snap, v, now), indent=2))
            return
        size = shutil.get_terminal_size((100, 45))
        _, _, lines = frame(Style(bool(use_color)), size, now)
        for ln in lines:
            print(ln)
        return

    import termios
    import tty
    sty = Style(True)
    fd = sys.stdin.fileno() if sys.stdin.isatty() else None
    old = termios.tcgetattr(fd) if fd is not None else None
    paused = False

    def cleanup(*_):
        net.stop = True
        if old is not None:
            termios.tcsetattr(fd, termios.TCSADRAIN, old)
        sys.stdout.write("\033[?25h\033[?1049l")
        sys.stdout.flush()

    signal.signal(signal.SIGTERM, lambda *_: (cleanup(), sys.exit(0)))
    sys.stdout.write("\033[?1049h\033[?25l")
    try:
        if fd is not None:
            tty.setcbreak(fd)
        while True:
            now = time.time()
            if not paused:
                size = shutil.get_terminal_size((100, 40))
                _, _, lines = frame(sty, size, now)
                lines.append(sty.dim("  q quit   u UTC/local   r refresh now   p pause" + ("   [PAUSED]" if paused else "")))
                out = "\033[H" + "".join(ln + "\033[K\n" for ln in lines[:size.lines - 1]) + "\033[J"
                sys.stdout.write(out)
                sys.stdout.flush()
            if fd is not None:
                r, _, _ = select.select([sys.stdin], [], [], args.interval)
                if r:
                    k = sys.stdin.read(1)
                    if k in ("q", "Q", "\x03"):
                        break
                    if k in ("u", "U"):
                        args.utc = not args.utc
                        tz = timezone.utc if args.utc else datetime.now().astimezone().tzinfo
                    elif k in ("r", "R"):
                        net.kick = True
                        if logdir:
                            reader.load()
                    elif k in ("p", "P"):
                        paused = not paused
            else:
                time.sleep(args.interval)
    except KeyboardInterrupt:
        pass
    finally:
        cleanup()


if __name__ == "__main__":
    main()
