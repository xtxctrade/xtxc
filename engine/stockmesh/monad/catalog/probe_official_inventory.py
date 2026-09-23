#!/usr/bin/env python3
"""Bounded, read-only verification of selected published Monad contracts."""

import hashlib
import json
import sys
import urllib.request
from pathlib import Path

RPC = "https://rpc.monad.xyz"
SYMBOLS = ("aNVDA", "aAAPL", "aSPY", "aGME", "aRKLB")
VENUES = ("anchored-stock-router", "anchored-cashier", "anchored-stock")
IMPLEMENTATION_SLOT = "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc"
ADMIN_SLOT = "0xb53127684a568b3173ae13b9f8a6016e243e63b6e8ee1178d6a717850b5d6103"


def rpc(method: str, params: list) -> str:
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    req = urllib.request.Request(RPC, data=body, headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=10) as response:
        value = json.load(response)
    if "error" in value or not isinstance(value.get("result"), str):
        raise RuntimeError(f"RPC {method} failed: {value.get('error')}")
    return value["result"]


def code(address: str, block: str) -> dict:
    raw = rpc("eth_getCode", [address, block])
    if raw == "0x":
        raise RuntimeError(f"no runtime at {address}")
    binary = bytes.fromhex(raw[2:])
    result = {"codeBytes": len(binary), "codeSha256": "0x" + hashlib.sha256(binary).hexdigest()}
    implementation = rpc("eth_getStorageAt", [address, IMPLEMENTATION_SLOT, block])
    admin = rpc("eth_getStorageAt", [address, ADMIN_SLOT, block])
    result["eip1967Implementation"] = "0x" + implementation[-40:] if int(implementation, 16) else None
    result["eip1967Admin"] = "0x" + admin[-40:] if int(admin, 16) else None
    if result["eip1967Implementation"]:
        raw_implementation = rpc("eth_getCode", [result["eip1967Implementation"], block])
        result["implementationCodeSha256"] = "0x" + hashlib.sha256(bytes.fromhex(raw_implementation[2:])).hexdigest()
    return result


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit("usage: probe_official_inventory.py REGISTRY_JSON")
    catalog = json.loads(Path(sys.argv[1]).read_text())
    chain = int(rpc("eth_chainId", []), 16)
    if chain != 143:
        raise RuntimeError(f"wrong chain {chain}")
    block = rpc("eth_blockNumber", [])
    tokens = {item["issuerProductId"]: item for item in catalog["tokenObservations"]}
    venues = {item["venueId"]: item for item in catalog["venueObservations"]}
    result = {"chainId": chain, "blockNumber": int(block, 16), "tokens": [], "venues": []}
    for symbol in SYMBOLS:
        item = tokens[symbol]
        address = item["tokenAddress"]
        decimals = int(rpc("eth_call", [{"to": address, "data": "0x313ce567"}, block]), 16)
        if decimals != item["tokenDecimals"]:
            raise RuntimeError(f"wrong decimals for {symbol}: {decimals}")
        result["tokens"].append({"symbol": symbol, "address": address, "decimals": decimals, **code(address, block)})
    for name in VENUES:
        item = venues[name]
        result["venues"].append({"name": name, "address": item["contract"], **code(item["contract"], block)})
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
