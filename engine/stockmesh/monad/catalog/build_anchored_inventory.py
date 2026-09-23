#!/usr/bin/env python3
"""Regenerate issuer-observed Monad stock identities from a saved official page.

This imports inventory only. It never creates admitted products or trading routes.
"""

import hashlib
import json
import re
import sys
from pathlib import Path

SOURCE = "https://docs.anchored.finance/getting-started/anchored-tokens"
RIGHTS = "https://docs.anchored.finance/getting-started/anchored-tokens"
ROW = re.compile(r"<tr><td>(a[A-Z0-9]+)</td><td>(0x[0-9a-fA-F]{40})</td></tr>")


def main() -> None:
    if len(sys.argv) != 3:
        raise SystemExit("usage: build_anchored_inventory.py SAVED_OFFICIAL_MARKDOWN CATALOG_DIR")
    snapshot = Path(sys.argv[1]).read_text()
    section = snapshot.split("### Tokenized stocks", 1)[1].split("### Tokenized funds", 1)[0]
    rows = ROW.findall(section)
    # Eighty is the verified September baseline, not a permanent product cap.
    # A smaller page is treated as a possibly incomplete fetch, while new
    # issuer-published rows can be incorporated without changing the parser.
    if len(rows) < 80 or len({symbol for symbol, _ in rows}) != len(rows) or len({address.lower() for _, address in rows}) != len(rows):
        raise SystemExit(f"expected at least 80 distinct published stock tokens; found {len(rows)}")
    catalog_dir = Path(sys.argv[2])
    catalog_path = catalog_dir / "registry.v1.json"
    catalog = json.loads(catalog_path.read_text())
    catalog["tokenObservations"] = [
        {
            "instrumentId": symbol[1:],
            "issuer": "Anchored Finance",
            "issuerProductId": symbol,
            "chainId": 143,
            "tokenAddress": address.lower(),
            "tokenDecimals": 18,
            "sourceUrl": SOURCE,
            "rightsSourceUrl": RIGHTS,
        }
        for symbol, address in rows
    ]
    if catalog["products"] or catalog["admittedVenues"]:
        raise SystemExit("inventory generator refuses a catalog with live admissions")
    encoded = (json.dumps(catalog, indent=2) + "\n").encode()
    catalog_path.write_bytes(encoded)
    manifest_path = catalog_dir / "deployment-manifest.v1.json"
    manifest = json.loads(manifest_path.read_text())
    if manifest["state"] != "DISABLED":
        raise SystemExit("inventory generator refuses an enabled deployment")
    manifest["catalogSha256"] = hashlib.sha256(encoded).hexdigest()
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"stock tokens={len(rows)} official_source_sha256={hashlib.sha256(snapshot.encode()).hexdigest()}")


if __name__ == "__main__":
    main()
