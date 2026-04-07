#!/usr/bin/env python3
import argparse
import json
import sys
from typing import Any, Dict, List, Optional, Tuple


def rpc_call(rpc_url: str, method: str, params: List[Any]) -> Any:
    payload = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    }
    try:
        import urllib.request

        req = urllib.request.Request(
            rpc_url,
            data=json.dumps(payload).encode("utf-8"),
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        with urllib.request.urlopen(req) as resp:
            raw = resp.read().decode("utf-8")
    except Exception as exc:
        raise RuntimeError(f"rpc request failed for {method}: {exc}") from exc

    try:
        parsed = json.loads(raw)
    except Exception as exc:
        raise RuntimeError(f"invalid rpc JSON for {method}: {raw}") from exc

    if parsed.get("error") is not None:
        raise RuntimeError(f"rpc error for {method}: {parsed['error']}")
    return parsed.get("result")


def hex_to_int(value: Any) -> Optional[int]:
    if value is None:
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, str):
        value = value.strip()
        if value.startswith("0x") or value.startswith("0X"):
            return int(value, 16)
        return int(value)
    raise ValueError(f"unsupported integer encoding: {value!r}")


def normalize_address(value: str) -> str:
    as_int = int(value, 16) if value.startswith("0x") else int(value)
    return f"0x{as_int:040x}"


def normalize_slot(value: str) -> str:
    as_int = int(value, 16) if value.startswith("0x") else int(value)
    return f"0x{as_int:064x}"


def safe_normalize_address(value: Any) -> Optional[str]:
    if not isinstance(value, str):
        return None
    try:
        return normalize_address(value).lower()
    except Exception:
        return None


def resolve_hash_from_sequence_block(
    rpc_url: str, sequence_number: Optional[int], tx: Dict[str, Any]
) -> Tuple[Optional[str], Optional[str]]:
    if sequence_number is None:
        return None, "artifact missing sequence_number for fallback lookup"

    block = rpc_call(rpc_url, "eth_getBlockByNumber", [hex(sequence_number), True])
    if not isinstance(block, dict):
        return None, f"eth_getBlockByNumber({sequence_number}) returned non-object"

    block_txs = block.get("transactions") or []
    if not isinstance(block_txs, list) or not block_txs:
        return None, f"block {sequence_number} contains no transactions"

    local_accounts = {
        safe_normalize_address(account.get("address"))
        for account in tx.get("accounts", [])
        if isinstance(account, dict)
    }
    local_accounts.discard(None)

    def tx_from_to_match(entry: Dict[str, Any]) -> bool:
        from_addr = safe_normalize_address(entry.get("from"))
        to_addr = safe_normalize_address(entry.get("to"))
        return from_addr in local_accounts and to_addr in local_accounts

    # Most robust match for local nitro deposits: type 0x64 from->to matches
    exact = [
        entry
        for entry in block_txs
        if isinstance(entry, dict)
        and str(entry.get("type", "")).lower() == "0x64"
        and tx_from_to_match(entry)
    ]
    if len(exact) == 1:
        return exact[0].get("hash"), None
    if len(exact) > 1:
        return None, f"ambiguous fallback: {len(exact)} type=0x64 tx candidates in block {sequence_number}"

    by_accounts = [
        entry
        for entry in block_txs
        if isinstance(entry, dict) and tx_from_to_match(entry)
    ]
    if len(by_accounts) == 1:
        return by_accounts[0].get("hash"), None
    if len(by_accounts) > 1:
        return None, f"ambiguous fallback: {len(by_accounts)} account-matched tx candidates in block {sequence_number}"

    by_type = [
        entry
        for entry in block_txs
        if isinstance(entry, dict) and str(entry.get("type", "")).lower() == "0x64"
    ]
    if len(by_type) == 1:
        return by_type[0].get("hash"), None
    if len(by_type) > 1:
        return None, f"ambiguous fallback: {len(by_type)} type=0x64 txs in block {sequence_number}"

    return None, f"no fallback tx candidate found in block {sequence_number}"


def build_local_account_maps(accounts: List[Dict[str, Any]]) -> Dict[str, Dict[str, Any]]:
    out: Dict[str, Dict[str, Any]] = {}
    for account in accounts:
        addr = normalize_address(account["address"]).lower()
        storage_pre: Dict[str, int] = {}
        storage_post: Dict[str, int] = {}
        for slot_diff in account.get("storage", []):
            slot_key = normalize_slot(slot_diff["slot"]).lower()
            storage_pre[slot_key] = int(slot_diff["pre"], 16)
            storage_post[slot_key] = int(slot_diff["post"], 16)

        out[addr] = {
            "balance_pre": int(account["balance_pre"], 16),
            "balance_post": int(account["balance_post"], 16),
            "nonce_pre": int(account["nonce_pre"]),
            "nonce_post": int(account["nonce_post"]),
            "storage_pre": storage_pre,
            "storage_post": storage_post,
        }
    return out


def compare_receipt(tx: Dict[str, Any], receipt: Dict[str, Any]) -> List[str]:
    errs: List[str] = []

    expected_success = bool(tx["success"])
    actual_success = hex_to_int(receipt.get("status")) == 1
    if expected_success != actual_success:
        errs.append(
            f"receipt status mismatch for {tx.get('tx_hash')}: expected success={expected_success}, got success={actual_success}"
        )

    expected_gas = int(tx["gas_used"])
    actual_gas = hex_to_int(receipt.get("gasUsed"))
    if actual_gas is None:
        errs.append(f"receipt missing gasUsed for {tx.get('tx_hash')}")
    elif expected_gas != actual_gas:
        errs.append(
            f"gas mismatch for {tx.get('tx_hash')}: expected {expected_gas}, got {actual_gas}"
        )

    return errs


def compare_prestate_trace(tx: Dict[str, Any], trace: Dict[str, Any]) -> List[str]:
    errs: List[str] = []
    local_accounts = build_local_account_maps(tx.get("accounts", []))

    tracer_pre = trace.get("pre") or {}
    tracer_post = trace.get("post") or {}

    for addr, traced_post in tracer_post.items():
        addr_norm = normalize_address(addr).lower()
        local = local_accounts.get(addr_norm)
        if local is None:
            errs.append(f"post account {addr_norm} missing in local diff for {tx.get('tx_hash')}")
            continue

        if traced_post.get("balance") is not None:
            got = local["balance_post"]
            want = hex_to_int(traced_post.get("balance"))
            if want is not None and got != want:
                errs.append(
                    f"post balance mismatch {addr_norm}: expected {want}, got {got}"
                )

        if traced_post.get("nonce") is not None:
            got = local["nonce_post"]
            want = hex_to_int(traced_post.get("nonce"))
            if want is not None and got != want:
                errs.append(f"post nonce mismatch {addr_norm}: expected {want}, got {got}")

        traced_post_storage: Dict[str, Any] = traced_post.get("storage") or {}
        for slot, value in traced_post_storage.items():
            slot_norm = normalize_slot(slot).lower()
            want = hex_to_int(value)
            got = local["storage_post"].get(slot_norm)
            if want is not None and got != want:
                errs.append(
                    f"post storage mismatch {addr_norm}[{slot_norm}]: expected {want}, got {got}"
                )

    for addr, traced_pre in tracer_pre.items():
        addr_norm = normalize_address(addr).lower()
        local = local_accounts.get(addr_norm)
        if local is None:
            # prestate tracer can include read-only accounts we won't have in write-diff.
            continue

        if traced_pre.get("balance") is not None:
            got = local["balance_pre"]
            want = hex_to_int(traced_pre.get("balance"))
            if want is not None and got != want:
                errs.append(
                    f"pre balance mismatch {addr_norm}: expected {want}, got {got}"
                )

        if traced_pre.get("nonce") is not None:
            got = local["nonce_pre"]
            want = hex_to_int(traced_pre.get("nonce"))
            if want is not None and got != want:
                errs.append(f"pre nonce mismatch {addr_norm}: expected {want}, got {got}")

        traced_pre_storage: Dict[str, Any] = traced_pre.get("storage") or {}
        for slot, value in traced_pre_storage.items():
            slot_norm = normalize_slot(slot).lower()
            if slot_norm not in local["storage_pre"]:
                continue
            want = hex_to_int(value)
            got = local["storage_pre"].get(slot_norm)
            if want is not None and got != want:
                errs.append(
                    f"pre storage mismatch {addr_norm}[{slot_norm}]: expected {want}, got {got}"
                )

    return errs


def verify(rpc_url: str, artifact_path: str) -> Tuple[bool, List[str]]:
    with open(artifact_path, "r", encoding="utf-8") as handle:
        artifact = json.load(handle)

    errors: List[str] = []
    txs: List[Dict[str, Any]] = artifact.get("transactions", [])
    sequence_number = hex_to_int(artifact.get("sequence_number"))

    if not txs:
        errors.append("artifact contains no user transactions")
        return False, errors

    for tx in txs:
        tx_hash = tx.get("tx_hash")
        if not tx_hash:
            errors.append("transaction entry missing tx_hash")
            continue

        resolved_hash = tx_hash
        receipt = rpc_call(rpc_url, "eth_getTransactionReceipt", [resolved_hash])
        if receipt is None:
            fallback_hash, fallback_error = resolve_hash_from_sequence_block(
                rpc_url, sequence_number, tx
            )
            if fallback_hash and fallback_hash != tx_hash:
                resolved_hash = fallback_hash
                receipt = rpc_call(rpc_url, "eth_getTransactionReceipt", [resolved_hash])
            if receipt is None:
                if fallback_error:
                    errors.append(
                        f"missing receipt for {tx_hash}; fallback failed: {fallback_error}"
                    )
                else:
                    errors.append(f"missing receipt for {tx_hash}")
                continue

        tx_for_compare = dict(tx)
        tx_for_compare["tx_hash"] = resolved_hash
        errors.extend(compare_receipt(tx_for_compare, receipt))

        trace = rpc_call(
            rpc_url,
            "debug_traceTransaction",
            [
                resolved_hash,
                {
                    "tracer": "prestateTracer",
                    "tracerConfig": {"diffMode": True},
                },
            ],
        )
        if not isinstance(trace, dict):
            errors.append(f"unexpected trace shape for {resolved_hash}: {trace!r}")
            continue

        errors.extend(compare_prestate_trace(tx_for_compare, trace))

    return len(errors) == 0, errors


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Verify arb-revm dump artifact against Nitro receipts + prestate tracer"
    )
    parser.add_argument("--rpc-url", required=True, help="Nitro RPC URL")
    parser.add_argument("--artifact", required=True, help="Path to arb-revm --dump-diff JSON")
    args = parser.parse_args()

    ok, errors = verify(args.rpc_url, args.artifact)
    if ok:
        print("verification passed")
        return 0

    print("verification failed")
    for err in errors:
        print(f"- {err}")
    return 1


if __name__ == "__main__":
    sys.exit(main())
