#!/usr/bin/env python3
"""Refresh public Monday market candidates against issuer-published token IDs.

The issuer's token list proves identity; a venue article proves only a
historical market announcement. Neither proves a current executable route.
"""

import hashlib
import html
import json
import re
import sys
from pathlib import Path

SOURCE = "https://blog.monday.trade/rwas-are-live-on-monday-trade/"
TAG = re.compile(r"<[^>]+>")
ITEM = re.compile(r"<li>(.*?)</li>", re.DOTALL)
SYMBOL = re.compile(r"\(([a]?[A-Z0-9]+)\)$")


def main() -> None:
    if len(sys.argv) != 3:
        raise SystemExit("usage: build_monday_market_candidates.py SAVED_OFFICIAL_HTML CATALOG_DIR")
    snapshot = Path(sys.argv[1]).read_text()
    if "RWA Pairs:" not in snapshot or "RWA Campaigns" not in snapshot:
        raise SystemExit("official Monday market section missing")
    section = snapshot.split("RWA Pairs:", 1)[1].split("RWA Campaigns", 1)[0]
    symbols = []
    for raw in ITEM.findall(section):
        line = html.unescape(TAG.sub("", raw)).strip()
        match = SYMBOL.search(line)
        if match:
            symbols.append(match.group(1))
    if len(symbols) < 34 or len(set(symbols)) != len(symbols):
        raise SystemExit(f"expected at least 34 distinct published market names; found {len(symbols)}")

    catalog_dir = Path(sys.argv[2])
    catalog_path = catalog_dir / "registry.v1.json"
    catalog = json.loads(catalog_path.read_text())
    if catalog["products"] or catalog["admittedVenues"]:
        raise SystemExit("market discovery refuses a catalog with live admissions")
    issuer_tokens = {token["issuerProductId"]: token for token in catalog["tokenObservations"]}
    candidates = []
    for published in symbols:
        # The article writes Walmart as WMT; issuer documentation names aWMT.
        # Other unprefixed names must also resolve uniquely to an issuer token.
        product_id = published if published.startswith("a") else f"a{published}"
        token = issuer_tokens.get(product_id)
        if not token or token["issuer"] != "Anchored Finance":
            raise SystemExit(f"market name has no published issuer token: {published}")
        candidates.append({
            "instrumentId": token["instrumentId"],
            "issuer": token["issuer"],
            "sourceUrl": SOURCE,
        })
    if len({(item["instrumentId"], item["issuer"]) for item in candidates}) != len(candidates):
        raise SystemExit("duplicate economic instrument in market article")
    catalog["candidates"] = candidates
    encoded = (json.dumps(catalog, indent=2) + "\n").encode()
    catalog_path.write_bytes(encoded)
    manifest_path = catalog_dir / "deployment-manifest.v1.json"
    manifest = json.loads(manifest_path.read_text())
    if manifest["state"] != "DISABLED":
        raise SystemExit("market discovery refuses an enabled deployment")
    manifest["catalogSha256"] = hashlib.sha256(encoded).hexdigest()
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"venue-published candidates={len(candidates)} issuer tokens={len(issuer_tokens)}; no execution admission")


if __name__ == "__main__":
    main()
