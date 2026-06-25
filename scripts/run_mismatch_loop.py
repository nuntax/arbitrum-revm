#!/usr/bin/env python3
import argparse
import json
import os
import re
import shlex
import subprocess
import sys
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import Dict, List, Optional


DEV_PRIVKEY = "0xb6b15c8cb491557369f3c7d2c287b053eb229daa9c22138887752191c9520659"


@dataclass
class CasePreset:
    name: str
    required_kind: int
    bridge_command: str
    trigger_shell: bool


@dataclass
class RunResult:
    index: int
    ok: bool
    command: List[str]
    output: str
    run_dir: str
    log_path: str
    payload_path: str
    artifact_path: str
    sequence_number: Optional[int]
    started_at_utc: str
    finished_at_utc: str


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat()


def default_submit_retryable_command(arb_alloy_dir: str) -> str:
    quoted_dir = shlex.quote(arb_alloy_dir)
    return (
        f"cd {quoted_dir} && "
        f"DEV_PRIVKEY={DEV_PRIVKEY} "
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
    )


def extract_sequence_number(output: str) -> Optional[int]:
    match = re.search(r"captured sequence_number=(\d+)", output)
    if match is None:
        return None
    return int(match.group(1))


def write_text(path: str, text: str) -> None:
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(text)


def resolve_case(
    case: str,
    bridge_command: Optional[str],
    required_kind: Optional[int],
    trigger_shell: bool,
    workspace_dir: str,
) -> CasePreset:
    if case == "deposit":
        preset = CasePreset(
            name="deposit",
            required_kind=12,
            bridge_command="./test-node.bash script bridge-funds",
            trigger_shell=False,
        )
    elif case == "submit-retryable":
        preset = CasePreset(
            name="submit-retryable",
            required_kind=9,
            bridge_command=default_submit_retryable_command(
                os.path.join(workspace_dir, "arb-alloy")
            ),
            trigger_shell=True,
        )
    else:
        if required_kind is None:
            raise SystemExit("--required-kind is required when --case=custom")
        if bridge_command is None:
            raise SystemExit("--bridge-command is required when --case=custom")
        return CasePreset(
            name="custom",
            required_kind=required_kind,
            bridge_command=bridge_command,
            trigger_shell=trigger_shell,
        )

    if required_kind is not None:
        preset.required_kind = required_kind
    if bridge_command is not None:
        preset.bridge_command = bridge_command
    if trigger_shell:
        preset.trigger_shell = True
    return preset


def run_once(
    arb_revm_dir: str,
    rpc_url: str,
    feed_url: str,
    ready_timeout: float,
    capture_timeout: float,
    case: CasePreset,
    run_index: int,
    output_dir: str,
) -> RunResult:
    started_at = utc_now()
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S")
    run_dir = os.path.join(output_dir, f"run_{run_index:04d}_{stamp}")
    os.makedirs(run_dir, exist_ok=True)

    payload_path = os.path.join(run_dir, "payload.json")
    artifact_path = os.path.join(run_dir, "artifact.json")
    log_path = os.path.join(run_dir, "harness.log")

    cmd: List[str] = [
        "python3",
        "scripts/run_live_parity_harness.py",
        "--rpc-url",
        rpc_url,
        "--feed-url",
        feed_url,
        "--ready-timeout",
        str(ready_timeout),
        "--capture-timeout",
        str(capture_timeout),
        "--required-kind",
        str(case.required_kind),
        "--payload-out",
        payload_path,
        "--artifact-out",
        artifact_path,
        "--bridge-command",
        case.bridge_command,
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

    output = proc.stdout
    sequence_number = extract_sequence_number(output)
    command_line = " ".join(shlex.quote(part) for part in cmd)
    log_body = (
        f"started_at_utc={started_at}\n"
        f"command={command_line}\n"
        "========================================\n"
        f"{output}"
    )
    write_text(log_path, log_body)

    return RunResult(
        index=run_index,
        ok=proc.returncode == 0,
        command=cmd,
        output=output,
        run_dir=run_dir,
        log_path=log_path,
        payload_path=payload_path,
        artifact_path=artifact_path,
        sequence_number=sequence_number,
        started_at_utc=started_at,
        finished_at_utc=utc_now(),
    )


def append_report(report_path: str, run: RunResult, case: CasePreset) -> None:
    report: Dict[str, object]
    if os.path.exists(report_path):
        with open(report_path, "r", encoding="utf-8") as handle:
            report = json.load(handle)
    else:
        report = {
            "created_at_utc": utc_now(),
            "case": case.name,
            "required_kind": case.required_kind,
            "runs": [],
        }
    runs = report.setdefault("runs", [])
    if not isinstance(runs, list):
        raise RuntimeError("report JSON corrupted: runs is not a list")

    runs.append(
        {
            "index": run.index,
            "ok": run.ok,
            "sequence_number": run.sequence_number,
            "run_dir": run.run_dir,
            "payload_path": run.payload_path,
            "artifact_path": run.artifact_path,
            "log_path": run.log_path,
            "started_at_utc": run.started_at_utc,
            "finished_at_utc": run.finished_at_utc,
        }
    )
    report["updated_at_utc"] = utc_now()
    with open(report_path, "w", encoding="utf-8") as handle:
        json.dump(report, handle, indent=2)
        handle.write("\n")


def replay_command(rpc_url: str, run: RunResult) -> str:
    parts = [
        "cargo",
        "run",
        "-p",
        "arb-revm",
        "--",
        rpc_url,
        "seq-prev",
        run.payload_path,
    ]
    if run.sequence_number is not None:
        parts.extend(["--sequence-number", str(run.sequence_number)])
    parts.extend(["--dump-diff", run.artifact_path])
    return " ".join(shlex.quote(part) for part in parts)


def verify_command(rpc_url: str, run: RunResult) -> str:
    parts = [
        "python3",
        "scripts/verify_with_nitro.py",
        "--rpc-url",
        rpc_url,
        "--artifact",
        run.artifact_path,
    ]
    return " ".join(shlex.quote(part) for part in parts)


def main() -> int:
    script_dir = os.path.dirname(os.path.abspath(__file__))
    arb_revm_dir = os.path.dirname(script_dir)
    workspace_dir = os.path.dirname(arb_revm_dir)

    parser = argparse.ArgumentParser(
        description=(
            "Iterate live parity runs until a mismatch is found, preserving repro artifacts per run."
        )
    )
    parser.add_argument("--rpc-url", default="http://127.0.0.1:8547")
    parser.add_argument("--feed-url", default="ws://127.0.0.1:9642")
    parser.add_argument("--ready-timeout", type=float, default=20.0)
    parser.add_argument("--capture-timeout", type=float, default=120.0)
    parser.add_argument(
        "--case",
        choices=["deposit", "submit-retryable", "custom"],
        default="deposit",
    )
    parser.add_argument("--required-kind", type=int, default=None)
    parser.add_argument("--bridge-command", default=None)
    parser.add_argument("--trigger-shell", action="store_true")
    parser.add_argument(
        "--max-runs",
        type=int,
        default=0,
        help="0 means run until first failure",
    )
    parser.add_argument(
        "--continue-on-failure",
        action="store_true",
        help="Keep iterating even after a failing run.",
    )
    parser.add_argument("--sleep-seconds", type=float, default=0.0)
    parser.add_argument(
        "--output-dir",
        default="/tmp/arb_revm_mismatch_loop",
    )
    args = parser.parse_args()

    case = resolve_case(
        case=args.case,
        bridge_command=args.bridge_command,
        required_kind=args.required_kind,
        trigger_shell=args.trigger_shell,
        workspace_dir=workspace_dir,
    )

    os.makedirs(args.output_dir, exist_ok=True)
    report_path = os.path.join(args.output_dir, "report.json")

    print(
        f"starting mismatch loop case={case.name} kind={case.required_kind} "
        f"output_dir={args.output_dir}"
    )

    run_index = 1
    failures = 0
    while True:
        if args.max_runs > 0 and run_index > args.max_runs:
            break

        print(f"\n=== run {run_index} ===")
        run = run_once(
            arb_revm_dir=arb_revm_dir,
            rpc_url=args.rpc_url,
            feed_url=args.feed_url,
            ready_timeout=args.ready_timeout,
            capture_timeout=args.capture_timeout,
            case=case,
            run_index=run_index,
            output_dir=args.output_dir,
        )
        append_report(report_path, run, case)

        status = "PASS" if run.ok else "FAIL"
        print(
            f"[{status}] run={run.index} seq={run.sequence_number} "
            f"payload={run.payload_path} artifact={run.artifact_path} log={run.log_path}"
        )

        if not run.ok:
            failures += 1
            print("\nreplay command:")
            print(f"  (cd {shlex.quote(arb_revm_dir)} && {replay_command(args.rpc_url, run)})")
            print("verify command:")
            print(f"  (cd {shlex.quote(arb_revm_dir)} && {verify_command(args.rpc_url, run)})")
            if not args.continue_on_failure:
                print("\nstopping after first failure (default behavior).")
                break

        run_index += 1
        if args.sleep_seconds > 0:
            time.sleep(args.sleep_seconds)

    print(
        f"\ncompleted runs={run_index - 1} failures={failures} report={report_path}"
    )
    return 0 if failures == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
