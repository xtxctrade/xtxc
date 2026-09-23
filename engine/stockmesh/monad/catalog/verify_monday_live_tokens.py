#!/usr/bin/env python3
"""Bounded read-only Monad checks for venue-displayed token identities.

This verifies chain, deployed code, ERC-20 decimals and symbol. It does not
verify issuance rights, order admission, liquidity or customer eligibility.
"""

import hashlib
import json
import sys
import time
import urllib.request
from pathlib import Path

RPC = "https://rpc.monad.xyz"
DECIMALS = "0x313ce567"
SYMBOL = "0x95d89b41"


def rpc_batch(items):
    request = urllib.request.Request(
        RPC,
        json.dumps(items).encode(),
        {"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(request, timeout=20) as response:
        replies = json.load(response)
    if not isinstance(replies, list) or len(replies) != len(items):
        raise ValueError("incomplete Monad RPC batch")
    by_id = {reply["id"]: reply for reply in replies}
    if len(by_id) != len(items):
        raise ValueError("duplicate Monad RPC reply ID")
    for item in items:
        reply = by_id.get(item["id"])
        if not reply or "error" in reply or not isinstance(reply.get("result"), str):
            raise ValueError(f"Monad RPC read failed: {item['id']}")
    return by_id


def decode_symbol(raw):
    data = bytes.fromhex(raw.removeprefix("0x"))
    if len(data) == 32:
        return data.rstrip(b"\0").decode()
    if len(data) < 64 or int.from_bytes(data[:32], "big") != 32:
        raise ValueError("invalid ERC-20 symbol encoding")
    length = int.from_bytes(data[32:64], "big")
    if not 1 <= length <= 32 or len(data) < 64 + length:
        raise ValueError("invalid ERC-20 symbol length")
    return data[64 : 64 + length].decode()


def main():
    if len(sys.argv) != 3:
        raise SystemExit("usage: verify_monday_live_tokens.py SNAPSHOT_JSON OUTPUT_JSON")
    source = Path(sys.argv[1])
    snapshot = json.loads(source.read_text())
    entries = snapshot["newTokenDetails"]
    if len(entries) != 32 or len({row["ticker"] for row in entries}) != 32:
        raise SystemExit("expected 32 distinct newly observed tokens")
    if len(snapshot["marketTickers"]) != 112 or len(set(snapshot["marketTickers"])) != 112:
        raise SystemExit("expected 112 distinct venue markets")
    if not all(row["ticker"] in snapshot["marketTickers"] for row in entries):
        raise SystemExit("token is not in observed venue list")

    chain = rpc_batch([{"jsonrpc": "2.0", "id": "chain", "method": "eth_chainId", "params": []}])
    if int(chain["chain"]["result"], 16) != 143:
        raise SystemExit("RPC is not Monad mainnet")

    verified = []
    for offset in range(0, len(entries), 8):
        batch = entries[offset : offset + 8]
        calls = []
        for row in batch:
            ticker, address = row["ticker"], row["tokenAddress"]
            if not address.startswith("0x") or len(address) != 42:
                raise ValueError(f"invalid address: {ticker}")
            calls.extend([
                {"jsonrpc": "2.0", "id": f"{ticker}:code", "method": "eth_getCode", "params": [address, "latest"]},
                {"jsonrpc": "2.0", "id": f"{ticker}:decimals", "method": "eth_call", "params": [{"to": address, "data": DECIMALS}, "latest"]},
                {"jsonrpc": "2.0", "id": f"{ticker}:symbol", "method": "eth_call", "params": [{"to": address, "data": SYMBOL}, "latest"]},
            ])
        replies = rpc_batch(calls)
        for row in batch:
            ticker = row["ticker"]
            code = replies[f"{ticker}:code"]["result"]
            decimals = int(replies[f"{ticker}:decimals"]["result"], 16)
            symbol = decode_symbol(replies[f"{ticker}:symbol"]["result"])
            if code == "0x" or not 0 <= decimals <= 18 or symbol != f"a{ticker}":
                raise ValueError(f"token identity mismatch: {ticker}")
            verified.append({
                "ticker": ticker,
                "tokenAddress": row["tokenAddress"],
                "tokenDecimals": decimals,
                "symbol": symbol,
                "runtimeCodeSha256": hashlib.sha256(bytes.fromhex(code[2:])).hexdigest(),
            })
        if offset + 8 < len(entries):
            time.sleep(1.1)
    result = {
        "schema": "xtxc.monad.read-only-token-check/v1",
        "chainId": 143,
        "rpc": RPC,
        "sourceSha256": hashlib.sha256(source.read_bytes()).hexdigest(),
        "tokens": verified,
    }
    Path(sys.argv[2]).write_text(json.dumps(result, indent=2) + "\n")
    print(f"verified deployed token identities={len(verified)}; no trade or issuer-rights admission")


if __name__ == "__main__":
    main()
