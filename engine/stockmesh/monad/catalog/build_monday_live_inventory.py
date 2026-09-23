#!/usr/bin/env python3
"""Merge observed Monday markets and checked token identities into PR01 inventory.

The output remains discovery-only. This tool cannot admit a venue or product.
"""

import hashlib
import json
import sys
from pathlib import Path


def fail(message):
    raise SystemExit(message)


def main():
    if len(sys.argv) != 4:
        fail("usage: build_monday_live_inventory.py OBSERVATION_JSON ONCHAIN_CHECK_JSON CATALOG_DIR")
    observation_path, check_path, catalog_dir = map(Path, sys.argv[1:])
    raw_observation = observation_path.read_bytes()
    observation = json.loads(raw_observation)
    checks = json.loads(check_path.read_text())
    tickers = observation["marketTickers"]
    new_details = observation["newTokenDetails"]
    if len(tickers) != 112 or len(set(tickers)) != 112:
        fail("expected exactly 112 distinct currently observed markets")
    if len(new_details) != 32 or len({row["ticker"] for row in new_details}) != 32:
        fail("expected 32 distinct new token detail pages")
    if checks["schema"] != "xtxc.monad.read-only-token-check/v1" or checks["chainId"] != 143:
        fail("invalid on-chain evidence schema or chain")
    if checks["sourceSha256"] != hashlib.sha256(raw_observation).hexdigest():
        fail("on-chain evidence was not generated from this observation")
    verified = {item["ticker"]: item for item in checks["tokens"]}
    if len(verified) != 32 or set(verified) != {item["ticker"] for item in new_details}:
        fail("missing, duplicate or extraneous on-chain token checks")

    catalog_path = catalog_dir / "registry.v1.json"
    manifest_path = catalog_dir / "deployment-manifest.v1.json"
    catalog = json.loads(catalog_path.read_text())
    manifest = json.loads(manifest_path.read_text())
    if catalog["chain"]["chainId"] != 143 or catalog["products"] or catalog["admittedVenues"]:
        fail("refusing to modify a non-discovery catalog")
    if manifest["state"] != "DISABLED":
        fail("refusing to modify an enabled deployment")
    existing = {item["instrumentId"]: item for item in catalog["tokenObservations"]}
    if len(existing) not in (80, 112) or set(existing) - set(tickers):
        fail("existing inventory does not match observed Monday markets")
    if len(existing) == 80 and set(existing) & set(verified):
        fail("new token overlaps the original issuer inventory")
    if len(existing) == 112 and set(existing) != set(tickers):
        fail("previous live inventory does not match this observation")

    observed_addresses = {item["tokenAddress"].lower() for item in existing.values()}
    for row in new_details:
        ticker, address = row["ticker"], row["tokenAddress"].lower()
        verified_row = verified[ticker]
        if (
            ticker not in tickers
            or verified_row["tokenAddress"].lower() != address
            or verified_row["symbol"] != f"a{ticker}"
            or verified_row["tokenDecimals"] != 18
            or len(verified_row["runtimeCodeSha256"]) != 64
        ):
            fail(f"unverified token identity: {ticker}")
        if ticker in existing:
            if existing[ticker]["tokenAddress"].lower() != address:
                fail(f"token address changed since previous inventory: {ticker}")
            continue
        if address in observed_addresses:
            fail(f"duplicate token address: {ticker}")
        observed_addresses.add(address)
        existing[ticker] = {
            "instrumentId": ticker,
            "issuer": "Anchored Finance",
            "issuerProductId": f"a{ticker}",
            "chainId": 143,
            "tokenAddress": address,
            "tokenDecimals": 18,
            "sourceUrl": f"https://app.monday.trade/#/rwa/a{ticker}",
            "rightsSourceUrl": "https://docs.anchored.finance/getting-started/anchored-tokens",
        }
    if set(existing) != set(tickers):
        fail("observed markets and token identities are not one-to-one")

    catalog["tokenObservations"] = [existing[ticker] for ticker in tickers]
    catalog["candidates"] = [
        {
            "instrumentId": ticker,
            "issuer": "Anchored Finance",
            "sourceUrl": f"https://app.monday.trade/#/rwa/a{ticker}",
        }
        for ticker in tickers
    ]
    encoded = (json.dumps(catalog, indent=2) + "\n").encode()
    catalog_path.write_bytes(encoded)
    manifest["catalogSha256"] = hashlib.sha256(encoded).hexdigest()
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"observed markets={len(tickers)} deployed token identities={len(existing)}; admitted=0")


if __name__ == "__main__":
    main()
