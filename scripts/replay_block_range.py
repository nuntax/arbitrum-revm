#!/usr/bin/env python3
import argparse
import json
import os
import shlex
import subprocess
import sys
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import List, Optional


@dataclass
class BlockRunResult:
    block_number: int
    ok: bool
    command: List[str]
    return_code: int
    output: str
    started_at_utc: str
    finished_at_utc: str
    log_path: str


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat()


def run_block(
    arb_revm_dir: str,
    rpc_url: str,
    block_number: int,
    fail_fast_inner: bool,
    skip_system_sender: bool,
    output_dir: str,
) -> BlockRunResult:
    started_at = utc_now()
    log_path = os.path.join(output_dir, f"block_{block_number}.log")

    cmd: List[str] = [
        "cargo",
        "run",
        "-p",
        "arb-revm",
        "--bin",
        "replay_block",
        "--",
        rpc_url,
        str(block_number),
    ]
    if fail_fast_inner:
        cmd.append("--fail-fast")
    if skip_system_sender:
        cmd.append("--skip-system-sender")

    proc = subprocess.run(
        cmd,
        cwd=arb_revm_dir,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )

    command_line = " ".join(shlex.quote(part) for part in cmd)
    log_body = (
        f"started_at_utc={started_at}\n"
        f"command={command_line}\n"
        "========================================\n"
        f"{proc.stdout}"
    )
    with open(log_path, "w", encoding="utf-8") as handle:
        handle.write(log_body)

    return BlockRunResult(
        block_number=block_number,
        ok=proc.returncode == 0,
        command=cmd,
        return_code=proc.returncode,
        output=proc.stdout,
        started_at_utc=started_at,
        finished_at_utc=utc_now(),
        log_path=log_path,
    )


def write_report(
    report_path: str,
    rpc_url: str,
    start_block: int,
    end_block: int,
    results: List[BlockRunResult],
) -> None:
    report = {
        "generated_at_utc": utc_now(),
        "rpc_url": rpc_url,
        "start_block": start_block,
        "end_block": end_block,
        "total_blocks": len(results),
        "passed_blocks": sum(1 for result in results if result.ok),
        "failed_blocks": sum(1 for result in results if not result.ok),
        "results": [
            {
                "block_number": result.block_number,
                "ok": result.ok,
                "return_code": result.return_code,
                "started_at_utc": result.started_at_utc,
                "finished_at_utc": result.finished_at_utc,
                "log_path": result.log_path,
            }
            for result in results
        ],
    }
    with open(report_path, "w", encoding="utf-8") as handle:
        json.dump(report, handle, indent=2)
        handle.write("\n")


def parse_report_out(output_dir: str, report_out: Optional[str]) -> str:
    if report_out is not None:
        return report_out
    return os.path.join(output_dir, "report.json")


def main() -> int:
    script_dir = os.path.dirname(os.path.abspath(__file__))
    arb_revm_dir = os.path.dirname(script_dir)

    parser = argparse.ArgumentParser(
        description="Replay and validate a full inclusive block range with replay_block."
    )
    parser.add_argument("--rpc-url", required=True)
    parser.add_argument("--start-block", type=int, required=True)
    parser.add_argument("--end-block", type=int, required=True)
    parser.add_argument(
        "--continue-on-failure",
        action="store_true",
        help="Keep running after failed blocks. Default stops at first failure.",
    )
    parser.add_argument(
        "--fail-fast-inner",
        action="store_true",
        help="Pass --fail-fast into replay_block per block.",
    )
    parser.add_argument(
        "--skip-system-sender",
        action="store_true",
        help="Pass --skip-system-sender into replay_block per block.",
    )
    parser.add_argument("--sleep-seconds", type=float, default=0.0)
    parser.add_argument("--output-dir", default="/tmp/arb_revm_replay_block_range")
    parser.add_argument("--report-out", default=None)
    args = parser.parse_args()

    if args.start_block < 1:
        raise SystemExit("--start-block must be >= 1")
    if args.end_block < args.start_block:
        raise SystemExit("--end-block must be >= --start-block")
    if args.sleep_seconds < 0:
        raise SystemExit("--sleep-seconds must be >= 0")

    os.makedirs(args.output_dir, exist_ok=True)
    report_path = parse_report_out(args.output_dir, args.report_out)

    results: List[BlockRunResult] = []
    block_count = args.end_block - args.start_block + 1
    print(
        f"starting range replay blocks={args.start_block}..{args.end_block} "
        f"count={block_count} output_dir={args.output_dir}"
    )

    for block_number in range(args.start_block, args.end_block + 1):
        print(f"\n=== block {block_number} ===")
        result = run_block(
            arb_revm_dir=arb_revm_dir,
            rpc_url=args.rpc_url,
            block_number=block_number,
            fail_fast_inner=args.fail_fast_inner,
            skip_system_sender=args.skip_system_sender,
            output_dir=args.output_dir,
        )
        results.append(result)

        status = "PASS" if result.ok else "FAIL"
        print(
            f"[{status}] block={result.block_number} return_code={result.return_code} log={result.log_path}"
        )

        if not result.ok and not args.continue_on_failure:
            print("stopping after first failure (default behavior).")
            break

        if args.sleep_seconds > 0:
            time.sleep(args.sleep_seconds)

    write_report(
        report_path=report_path,
        rpc_url=args.rpc_url,
        start_block=args.start_block,
        end_block=args.end_block,
        results=results,
    )

    passed = sum(1 for result in results if result.ok)
    total = len(results)
    print(f"\nsummary: {passed}/{total} blocks passing")
    print(f"report: {report_path}")

    failed_blocks = [result.block_number for result in results if not result.ok]
    if failed_blocks:
        print("failed blocks: " + ", ".join(str(block) for block in failed_blocks))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
