#!/usr/bin/env python3
"""Solend read-only spike — Part 2 schema validation.

Fetches a Solend mainnet obligation + its referenced reserves + oracles via
public RPC, decodes the on-chain bytes against known Solend / Pyth layouts,
and prints a mapping between the raw protocol shape and Part 2's
LendingSnapshot concepts (obligation, reserves, oracles, freshness
descriptors).

The script is stdlib-only. No pip installs. No signing, no tx, no keys.
"""

import argparse
import base64
import json
import struct
import sys
import time
import urllib.request
from datetime import datetime, timezone

# ── Minimal base58 (no pip) ────────────────────────────────────────────────
_B58 = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"

def b58encode(data: bytes) -> str:
    n = int.from_bytes(data, "big")
    out = bytearray()
    while n > 0:
        n, r = divmod(n, 58)
        out.append(_B58[r])
    # leading zero bytes become leading '1's
    pad = 0
    for b in data:
        if b == 0:
            pad += 1
        else:
            break
    return ("1" * pad) + bytes(reversed(out)).decode("ascii")

def b58decode(s: str) -> bytes:
    n = 0
    for ch in s:
        n = n * 58 + _B58.index(ch.encode("ascii"))
    # count leading '1's
    pad = 0
    for ch in s:
        if ch == "1":
            pad += 1
        else:
            break
    body = n.to_bytes((n.bit_length() + 7) // 8, "big") if n else b""
    return b"\x00" * pad + body

def pk_str(raw32: bytes) -> str:
    assert len(raw32) == 32, f"pubkey must be 32 bytes, got {len(raw32)}"
    return b58encode(raw32)

# ── RPC ────────────────────────────────────────────────────────────────────
DEFAULT_RPC = "https://api.mainnet-beta.solana.com"

def rpc(url: str, method: str, params: list) -> dict:
    payload = {"jsonrpc": "2.0", "id": 1, "method": method, "params": params}
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode("utf-8"),
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=15) as resp:
        return json.loads(resp.read())

def get_account(url: str, pubkey: str):
    """Return (data_bytes, owner_pubkey, lamports, context_slot) or (None, None, None, slot)."""
    r = rpc(url, "getAccountInfo", [pubkey, {"encoding": "base64"}])
    v = r.get("result", {})
    slot = v.get("context", {}).get("slot")
    av = v.get("value")
    if not av:
        return None, None, None, slot
    data = base64.b64decode(av["data"][0])
    return data, av["owner"], av["lamports"], slot

def get_slot_blocktime(url: str, slot: int):
    """Return block-time (unix seconds) for a slot, or None."""
    try:
        r = rpc(url, "getBlockTime", [slot])
        return r.get("result")
    except Exception:
        return None

def fmt_unix(ts):
    if ts is None:
        return "(unknown)"
    return datetime.fromtimestamp(ts, tz=timezone.utc).isoformat().replace("+00:00", "Z")

# ── Solend Obligation decode (layout from solend-program source) ──────────
#
# Roughly matches Obligation::LEN = 1300 bytes in solend-program v3 / v4.
# Field offsets below come from Pack::unpack_unchecked in
# solend-program src/state/obligation.rs. Only the subset Part 2 cares about.
SOLEND_PROGRAM_ID = "So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo"

# Canonical layout cited from solend-program master
# (token-lending/program/src/state/obligation.rs, Pack impl):
#   0   1   u8       version
#   1   8   u64 LE   last_update.slot
#   9   1   bool     last_update.stale
#   10  32  Pubkey   lending_market
#   42  32  Pubkey   owner
#   74  16  Decimal  deposited_value
#   90  16  Decimal  borrowed_value
#   106 16  Decimal  allowed_borrow_value
#   122 16  Decimal  unhealthy_borrow_value
#   138 64  [u8;64]  _padding
#   202 1   u8       deposits_len
#   203 1   u8       borrows_len
#   204 1096         data_flat  (deposits[] followed by borrows[])
#
# ObligationCollateral (88 bytes): deposit_reserve(32) + deposited_amount(8)
#   + market_value(16) + _padding(32)
# ObligationLiquidity (112 bytes): borrow_reserve(32) + cumulative_borrow_rate_wads(16)
#   + borrowed_amount_wads(16) + market_value(16) + _padding(32)
OBLIGATION_LEN = 1300
OBLIGATION_COLLATERAL_LEN = 88
OBLIGATION_LIQUIDITY_LEN  = 112
DEPOSITS_LEN_OFFSET = 202

def decode_obligation(data: bytes) -> dict:
    if len(data) != OBLIGATION_LEN:
        raise ValueError(
            f"obligation data size {len(data)} != {OBLIGATION_LEN}; "
            f"layout assumptions will not apply"
        )
    version = data[0]
    last_update_slot = int.from_bytes(data[1:9], "little")
    last_update_stale = data[9]
    lending_market = pk_str(data[10:42])
    owner = pk_str(data[42:74])
    deposited_value = int.from_bytes(data[74:90], "little")
    borrowed_value = int.from_bytes(data[90:106], "little")
    allowed_borrow_value = int.from_bytes(data[106:122], "little")
    unhealthy_borrow_value = int.from_bytes(data[122:138], "little")
    padding = data[138:202]
    deposits_len = data[DEPOSITS_LEN_OFFSET]
    borrows_len = data[DEPOSITS_LEN_OFFSET + 1]
    arrays_off = DEPOSITS_LEN_OFFSET + 2

    deposits = []
    for i in range(deposits_len):
        base = arrays_off + i * OBLIGATION_COLLATERAL_LEN
        if base + OBLIGATION_COLLATERAL_LEN > len(data):
            break
        deposits.append({
            "deposit_reserve": pk_str(data[base:base+32]),
            "deposited_amount": int.from_bytes(data[base+32:base+40], "little"),
            "market_value": int.from_bytes(data[base+40:base+56], "little"),
        })

    borrows_start = arrays_off + deposits_len * OBLIGATION_COLLATERAL_LEN
    borrows = []
    for i in range(borrows_len):
        base = borrows_start + i * OBLIGATION_LIQUIDITY_LEN
        if base + OBLIGATION_LIQUIDITY_LEN > len(data):
            break
        borrows.append({
            "borrow_reserve": pk_str(data[base:base+32]),
            "cumulative_borrow_rate_wads": int.from_bytes(data[base+32:base+48], "little"),
            "borrowed_amount_wads": int.from_bytes(data[base+48:base+64], "little"),
            "market_value": int.from_bytes(data[base+64:base+80], "little"),
        })

    return {
        "version": version,
        "last_update_slot": last_update_slot,
        "last_update_stale": bool(last_update_stale),
        "lending_market": lending_market,
        "owner": owner,
        "deposited_value_scaled_1e18": deposited_value,
        "borrowed_value_scaled_1e18": borrowed_value,
        "allowed_borrow_value_scaled_1e18": allowed_borrow_value,
        "unhealthy_borrow_value_scaled_1e18": unhealthy_borrow_value,
        "padding_is_zero": all(b == 0 for b in padding),
        "deposits_len": deposits_len,
        "borrows_len": borrows_len,
        "deposits": deposits,
        "borrows": borrows,
        "raw_data_size": len(data),
    }

# ── Solend Reserve decode (subset) ────────────────────────────────────────
#
# Reserve::LEN = 619 (observed). Layout from solend-program src/state/reserve.rs.
# We decode the fields Part 2 cares about: last_update, lending_market, liquidity
# (mint, decimals, pyth/switchboard oracle pubkeys), and config risk params.

def decode_reserve(data: bytes) -> dict:
    if len(data) < 300:
        raise ValueError(f"reserve data too small: {len(data)} bytes")
    off = 0
    version = data[off]; off += 1
    last_update_slot = int.from_bytes(data[off:off+8], "little"); off += 8
    last_update_stale = data[off]; off += 1
    lending_market = pk_str(data[off:off+32]); off += 32
    # Liquidity block:
    liquidity_mint_pubkey = pk_str(data[off:off+32]); off += 32
    liquidity_mint_decimals = data[off]; off += 1
    liquidity_supply_pubkey = pk_str(data[off:off+32]); off += 32
    liquidity_pyth_oracle = pk_str(data[off:off+32]); off += 32
    liquidity_switchboard_oracle = pk_str(data[off:off+32]); off += 32
    available_amount = int.from_bytes(data[off:off+8], "little"); off += 8
    borrowed_amount_wads = int.from_bytes(data[off:off+16], "little"); off += 16
    cumulative_borrow_rate_wads = int.from_bytes(data[off:off+16], "little"); off += 16
    market_price_wads = int.from_bytes(data[off:off+16], "little"); off += 16
    smoothed_market_price_wads = int.from_bytes(data[off:off+16], "little"); off += 16
    # Skip rest of liquidity and go into collateral + config
    # We don't need to decode further for the spike's purpose

    return {
        "version": version,
        "last_update_slot": last_update_slot,
        "last_update_stale": bool(last_update_stale),
        "lending_market": lending_market,
        "liquidity": {
            "mint_pubkey": liquidity_mint_pubkey,
            "mint_decimals": liquidity_mint_decimals,
            "supply_pubkey": liquidity_supply_pubkey,
            "pyth_oracle": liquidity_pyth_oracle,
            "switchboard_oracle": liquidity_switchboard_oracle,
            "available_amount": available_amount,
            "borrowed_amount_wads": borrowed_amount_wads,
            "cumulative_borrow_rate_wads": cumulative_borrow_rate_wads,
            "market_price_wads": market_price_wads,
            "smoothed_market_price_wads": smoothed_market_price_wads,
        },
        "raw_data_size": len(data),
    }

NULL_PUBKEY = "11111111111111111111111111111111"

# ── Oracle probe ──────────────────────────────────────────────────────────
#
# We don't try to parse Pyth-Lazer / Switchboard On-Demand binary formats —
# that's substantial per-program decode work. Instead we fetch the account
# and report: owner program, size, and whether the account exists. The
# oracle program identity (owner) is the key "freshness source descriptor"
# Part 2 §12.2 relies on.

KNOWN_ORACLE_OWNERS = {
    "FsJ3A3u2vn5cTVofAjvy6y5kwABJAqYWpe4975bi2epH": "Pyth (legacy)",
    "pythWSnswVUd12oZpeFP8e9CVaEqJg25g1Vtc2biRsT": "Pyth (lazer router?)",
    "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ": "Pyth Lazer (receiver)",
    "SW1TCH7qEPTdLsDHRgPuMQjbQxKdH2aBStViMFnt64f": "Switchboard On-Demand",
    "SBondMDrcV3K4kxZR1HNVT7osZxAHVHgYXL5Ze1oMUv": "Switchboard (legacy)",
    "pythVfZmo6rJDJJvbBpvZ7Gj3ykxL23j1gYKj8s1TTi": "(unknown oracle shape)",
}

def probe_oracle(url: str, pubkey: str) -> dict:
    if pubkey == NULL_PUBKEY:
        return {"pubkey": pubkey, "status": "null_sentinel"}
    data, owner, lamports, ctx_slot = get_account(url, pubkey)
    if data is None:
        return {"pubkey": pubkey, "status": "not_found", "context_slot": ctx_slot}
    return {
        "pubkey": pubkey,
        "owner": owner,
        "owner_label": KNOWN_ORACLE_OWNERS.get(owner, "(unknown)"),
        "data_size": len(data),
        "lamports": lamports,
        "context_slot": ctx_slot,
        "status": "present",
    }

# ── Main ──────────────────────────────────────────────────────────────────

def main():
    p = argparse.ArgumentParser(description="Solend read-only spike")
    p.add_argument("--obligation", required=True,
                   help="Solend Obligation pubkey (mainnet)")
    p.add_argument("--rpc", default=DEFAULT_RPC,
                   help=f"RPC URL (default: {DEFAULT_RPC})")
    p.add_argument("--json", action="store_true",
                   help="emit JSON instead of human-readable output")
    args = p.parse_args()

    started = time.time()
    print(f"# Solend read-only spike")
    print(f"# RPC       : {args.rpc}")
    print(f"# obligation: {args.obligation}")
    print(f"# started   : {fmt_unix(started)}\n")

    # ── 1. Fetch obligation ──────────────────────────────────────────────
    obl_data, obl_owner, obl_lamports, obl_ctx_slot = get_account(args.rpc, args.obligation)
    if obl_data is None:
        print("FAIL: obligation account not found on mainnet.")
        sys.exit(2)
    if obl_owner != SOLEND_PROGRAM_ID:
        print(f"FAIL: account owner is {obl_owner}, not Solend.")
        sys.exit(2)

    obl_fetched_at = time.time()
    obligation = decode_obligation(obl_data)
    print(f"## Obligation ({args.obligation})")
    print(f"  version            : {obligation['version']}")
    print(f"  data size (bytes)  : {obligation['raw_data_size']}")
    print(f"  owner (wallet)     : {obligation['owner']}")
    print(f"  lending_market     : {obligation['lending_market']}")
    print(f"  last_update.slot   : {obligation['last_update_slot']}")
    print(f"  last_update.stale  : {obligation['last_update_stale']}")
    print(f"  deposits_len       : {obligation['deposits_len']}")
    print(f"  borrows_len        : {obligation['borrows_len']}")
    print(f"  fetched_at_slot    : {obl_ctx_slot}  (RPC context)")
    print(f"  fetched_at_walltime: {fmt_unix(obl_fetched_at)}")
    print()

    if obligation["deposits"]:
        print(f"  Deposits ({obligation['deposits_len']}):")
        for i, dep in enumerate(obligation["deposits"]):
            print(f"    [{i}] reserve={dep['deposit_reserve']}")
            print(f"        deposited_amount_collateral_units={dep['deposited_amount']}")
    if obligation["borrows"]:
        print(f"  Borrows ({obligation['borrows_len']}):")
        for i, bor in enumerate(obligation["borrows"]):
            print(f"    [{i}] reserve={bor['borrow_reserve']}")
            print(f"        borrowed_amount_wads={bor['borrowed_amount_wads']}")
    print()

    # Collect the distinct reserves referenced.
    reserve_refs = set()
    for dep in obligation["deposits"]:
        reserve_refs.add(dep["deposit_reserve"])
    for bor in obligation["borrows"]:
        reserve_refs.add(bor["borrow_reserve"])

    # ── 2. Fetch each referenced reserve ─────────────────────────────────
    reserves = {}
    for res_pk in sorted(reserve_refs):
        data, owner, lamports, ctx_slot = get_account(args.rpc, res_pk)
        if data is None or owner != SOLEND_PROGRAM_ID:
            print(f"## Reserve {res_pk} — NOT FOUND or not Solend-owned")
            continue
        fetched_at = time.time()
        try:
            reserve = decode_reserve(data)
        except Exception as e:
            print(f"## Reserve {res_pk} — decode failed: {e}")
            continue
        reserve["_fetched_at_walltime"] = fetched_at
        reserve["_context_slot"] = ctx_slot
        reserves[res_pk] = reserve
        liq = reserve["liquidity"]
        print(f"## Reserve {res_pk}")
        print(f"  version             : {reserve['version']}")
        print(f"  last_update.slot    : {reserve['last_update_slot']}")
        print(f"  last_update.stale   : {reserve['last_update_stale']}")
        print(f"  liquidity.mint      : {liq['mint_pubkey']}  (decimals={liq['mint_decimals']})")
        print(f"  liquidity.pyth      : {liq['pyth_oracle']}")
        print(f"  liquidity.switchbd  : {liq['switchboard_oracle']}")
        print(f"  market_price_wads   : {liq['market_price_wads']}")
        print(f"  borrowed_amt_wads   : {liq['borrowed_amount_wads']}")
        print(f"  available_amount    : {liq['available_amount']}")
        print(f"  fetched_at_slot     : {ctx_slot}  (RPC context)")
        print(f"  fetched_at_walltime : {fmt_unix(fetched_at)}")
        print()

    # ── 3. Probe oracles for each reserve ───────────────────────────────
    print("## Oracles referenced")
    oracle_probes = {}
    for res_pk, reserve in reserves.items():
        liq = reserve["liquidity"]
        for kind, oracle_pk in (("pyth", liq["pyth_oracle"]),
                                 ("switchboard", liq["switchboard_oracle"])):
            probe = probe_oracle(args.rpc, oracle_pk)
            probe["reserve"] = res_pk
            probe["kind"] = kind
            oracle_probes.setdefault(oracle_pk, probe)
            if probe["status"] == "null_sentinel":
                print(f"  [{kind}] {oracle_pk}  (NULL sentinel — not configured)")
            elif probe["status"] == "not_found":
                print(f"  [{kind}] {oracle_pk}  NOT FOUND on chain")
            else:
                print(f"  [{kind}] {oracle_pk}")
                print(f"      owner={probe['owner']}  ({probe['owner_label']})")
                print(f"      size={probe['data_size']} bytes")
    print()

    # ── 4. Mapping summary: raw state ⇢ Part 2 snapshot concepts ────────
    print("## Part 2 schema mapping summary\n")
    print(f"  snapshot.session.wallet (should match obligation.owner): {obligation['owner']}")
    print(f"  snapshot.protocol_tag               : 'solend'  (Solend mainnet lending program)")
    print(f"  snapshot.fetched_at_slot            : {obl_ctx_slot}  (RPC context at obligation fetch)")
    print(f"  snapshot.obligation.identifier       : {args.obligation}")
    print(f"  snapshot.obligation.owner           : {obligation['owner']}")
    print(f"  snapshot.obligation.protocol_last_updated_slot : {obligation['last_update_slot']}")
    print(f"  snapshot.obligation.lifecycle_markers:")
    print(f"      stale={obligation['last_update_stale']}  "
          f"padding_all_zero={obligation['padding_is_zero']}")
    print(f"  snapshot.reserves.count             : {len(reserves)}")
    for res_pk, reserve in reserves.items():
        liq = reserve["liquidity"]
        print(f"  snapshot.reserves[{res_pk[:8]}..]:")
        print(f"      mint          : {liq['mint_pubkey']}")
        print(f"      decimals      : {liq['mint_decimals']}")
        print(f"      protocol_last_updated_slot : {reserve['last_update_slot']}")
        print(f"      oracle_pyth   : {liq['pyth_oracle']}  (distinct from Part 2 \"single Oracle\" assumption)")
        print(f"      oracle_switchbd: {liq['switchboard_oracle']}")
    print(f"  snapshot.oracles.count              : {len(oracle_probes)}")
    for pk, probe in oracle_probes.items():
        if probe["status"] == "present":
            print(f"      {pk[:12]}..  owner={probe['owner_label']}  size={probe['data_size']}")

    duration = time.time() - started
    print(f"\n# spike finished in {duration:.2f}s")

    if args.json:
        blob = {
            "obligation": obligation,
            "obligation_fetch": {
                "pubkey": args.obligation,
                "context_slot": obl_ctx_slot,
                "fetched_at_walltime": obl_fetched_at,
            },
            "reserves": reserves,
            "oracle_probes": oracle_probes,
        }
        print("\n---JSON---")
        print(json.dumps(blob, indent=2, default=str))

if __name__ == "__main__":
    main()
