#!/usr/bin/env python3
import argparse
import json
import os
import random
import subprocess
import sys
from datetime import datetime, timezone
from dataclasses import dataclass
from typing import Dict, List


DEV_PRIVKEY = "0xb6b15c8cb491557369f3c7d2c287b053eb229daa9c22138887752191c9520659"


@dataclass
class CaseConfig:
    name: str
    kind: int
    trigger_cmd: str
    trigger_shell: bool


@dataclass
class CaseResult:
    name: str
    kind: int
    round_index: int
    ok: bool
    output: str
    payload_path: str
    artifact_path: str


def run_case(
    arb_revm_dir: str,
    case: CaseConfig,
    round_index: int,
    capture_timeout: float,
) -> CaseResult:
    payload_path = f"/tmp/live_parity_r{round_index}_{case.name}_payload.json"
    artifact_path = f"/tmp/live_parity_r{round_index}_{case.name}_artifact.json"
    cmd: List[str] = [
        "python3",
        "scripts/run_live_parity_harness.py",
        "--rpc-url",
        "http://127.0.0.1:8547",
        "--feed-url",
        "ws://127.0.0.1:9642",
        "--required-kind",
        str(case.kind),
        "--capture-timeout",
        str(capture_timeout),
        "--payload-out",
        payload_path,
        "--artifact-out",
        artifact_path,
        "--bridge-command",
        case.trigger_cmd,
    ]
    if case.trigger_shell:
        cmd.append("--trigger-shell")

    proc = subprocess.run(
        cmd,
        cwd=arb_revm_dir,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    return CaseResult(
        name=case.name,
        kind=case.kind,
        round_index=round_index,
        ok=proc.returncode == 0,
        output=proc.stdout,
        payload_path=payload_path,
        artifact_path=artifact_path,
    )


def write_report(path: str, rounds: int, results: List[CaseResult]) -> None:
    by_case: Dict[str, Dict[str, int]] = {}
    serialized_results = []
    for result in results:
        case_stats = by_case.setdefault(result.name, {"passed": 0, "failed": 0, "total": 0})
        case_stats["total"] += 1
        if result.ok:
            case_stats["passed"] += 1
        else:
            case_stats["failed"] += 1

        serialized_results.append(
            {
                "name": result.name,
                "kind": result.kind,
                "round_index": result.round_index,
                "ok": result.ok,
                "payload_path": result.payload_path,
                "artifact_path": result.artifact_path,
            }
        )

    report = {
        "generated_at_utc": datetime.now(timezone.utc).isoformat(),
        "rounds": rounds,
        "total_cases": len(results),
        "passed_cases": sum(1 for result in results if result.ok),
        "failed_cases": sum(1 for result in results if not result.ok),
        "by_case": by_case,
        "results": serialized_results,
    }
    with open(path, "w", encoding="utf-8") as handle:
        json.dump(report, handle, indent=2)
        handle.write("\n")


def main() -> int:
    script_dir = os.path.dirname(os.path.abspath(__file__))
    arb_revm_dir = os.path.dirname(script_dir)
    workspace_dir = os.path.dirname(arb_revm_dir)
    arb_alloy_dir = os.path.join(workspace_dir, "arb-alloy")

    parser = argparse.ArgumentParser(
        description="Run a live Nitro parity matrix over critical delayed tx classes."
    )
    parser.add_argument("--capture-timeout", type=float, default=180.0)
    parser.add_argument("--rounds", type=int, default=1)
    parser.add_argument("--shuffle", action="store_true")
    parser.add_argument("--fail-fast", action="store_true")
    parser.add_argument("--report-out", default="/tmp/live_parity_matrix_report.json")
    parser.add_argument(
        "--submit-retryable-command",
        default=(
            "cd {arb_alloy_dir} && "
            "DEV_PRIVKEY={dev_privkey} "
            "ARBITRUM_RPC=http://127.0.0.1:8547 "
            "ETHEREUM_RPC=http://127.0.0.1:8545 "
            "ROLLUP_ADDRESS=0x8C2005559c23b0cf55F545103eaf0460641F9609 "
            "INBOX_ADDRESS=0xA520303d332b27763E3518000861EdBc9843ceCc "
            "OUTBOX_ADDRESS=0x20024006909f7F5a45c0E5d27FA8BB0888e52E09 "
            "ROLLUP_EVENT_INBOX_ADDRESS=0xB0CDcE825c9a9BF066fD64495D5d1A569e6d51B0 "
            "CHALLENGE_MANAGER_ADDRESS=0x43679E89A6FeA7Df648350F5A7FDCD0bB4A501E9 "
            "ADMIN_PROXY_ADDRESS=0x275FC51309e5928Cb085b463ADEF5cbD45c76b62 "
            "SEQUENCER_INBOX_ADDRESS=0x60FFA00eaC35597FAAb2b2B5926e5b0CddF5700c "
            "BRIDGE_ADDRESS=0x6d2B5Db5290BE4Be47ACC104EB052eEFc1f7b1b1 "
            "VALIDATOR_WALLET_CREATOR_ADDRESS=0xcc5ad74674C356345Db88c354491C7d3173C56f5 "
            "cargo test -p arb-alloy-consensus "
            "submit_retryable_produces_submit_retryable_tx_on_l2 -- --nocapture"
        ).format(arb_alloy_dir=arb_alloy_dir, dev_privkey=DEV_PRIVKEY),
    )
    args = parser.parse_args()

    if args.rounds < 1:
        raise SystemExit("--rounds must be >= 1")

    cases: List[CaseConfig] = [
        CaseConfig(
            name="deposit_kind12",
            kind=12,
            trigger_cmd="./test-node.bash script bridge-funds",
            trigger_shell=False,
        ),
        CaseConfig(
            name="submit_retryable_kind9",
            kind=9,
            trigger_cmd=args.submit_retryable_command,
            trigger_shell=True,
        ),
    ]

    results: List[CaseResult] = []
    for round_index in range(1, args.rounds + 1):
        print(f"\n=== round {round_index}/{args.rounds} ===")
        round_cases = list(cases)
        if args.shuffle:
            random.shuffle(round_cases)
        for case in round_cases:
            print(f"\n=== running case: {case.name} (kind={case.kind}) round={round_index} ===")
            result = run_case(
                arb_revm_dir=arb_revm_dir,
                case=case,
                round_index=round_index,
                capture_timeout=args.capture_timeout,
            )
            print(result.output)
            print(
                f"[{case.name}] round={round_index} payload={result.payload_path} artifact={result.artifact_path} status={'PASS' if result.ok else 'FAIL'}"
            )
            results.append(result)
            if args.fail_fast and not result.ok:
                print("fail-fast enabled; stopping early after first failure")
                write_report(args.report_out, args.rounds, results)
                print(f"wrote report: {args.report_out}")
                return 1

    write_report(args.report_out, args.rounds, results)
    print(f"\nwrote report: {args.report_out}")

    passed = sum(1 for result in results if result.ok)
    total = len(results)
    print(f"\nparity matrix: {passed}/{total} passing")
    by_case: Dict[str, List[CaseResult]] = {}
    for result in results:
        by_case.setdefault(result.name, []).append(result)
    for name, case_results in by_case.items():
        case_passed = sum(1 for result in case_results if result.ok)
        print(f"- {name}: {case_passed}/{len(case_results)} passing")

    return 0 if passed == total else 1


if __name__ == "__main__":
    sys.exit(main())
