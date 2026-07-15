#!/usr/bin/env python3
import argparse
import json
import os
import shlex
import signal
import subprocess
import sys
import time
from select import select
from typing import Optional, Tuple


def run_cmd(
    cmd,
    cwd: str,
    check: bool = True,
    shell: bool = False,
) -> subprocess.CompletedProcess:
    proc = subprocess.run(
        cmd,
        cwd=cwd,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        shell=shell,
    )
    if check and proc.returncode != 0:
        raise RuntimeError(
            f"command failed ({proc.returncode}): {cmd if shell else ' '.join(cmd)}\n{proc.stdout}"
        )
    return proc


def read_line_with_timeout(
    proc: subprocess.Popen, timeout_sec: float
) -> Optional[str]:
    if proc.stdout is None:
        return None
    ready, _, _ = select([proc.stdout], [], [], timeout_sec)
    if not ready:
        return None
    line = proc.stdout.readline()
    if not line:
        return None
    return line


def wait_for_feed_ready(proc: subprocess.Popen, timeout_sec: float) -> None:
    deadline = time.monotonic() + timeout_sec
    while time.monotonic() < deadline:
        line = read_line_with_timeout(proc, 0.5)
        if line is None:
            continue
        sys.stdout.write(line)
        if "Created stream, starting to read messages" in line:
            return
    raise RuntimeError(
        f"timed out waiting for sequencer_client readiness after {timeout_sec}s"
    )


def extract_kind(payload: dict) -> Optional[int]:
    try:
        return int(
            payload["messages"][0]["message"]["message"]["header"]["kind"]  # type: ignore[index]
        )
    except Exception:
        return None


def extract_message_kind(message: dict) -> Optional[int]:
    try:
        return int(message["message"]["message"]["header"]["kind"])  # type: ignore[index]
    except Exception:
        return None


def capture_raw_payload(
    proc: subprocess.Popen,
    timeout_sec: float,
    required_kind: Optional[int],
    minimum_sequence: Optional[int],
) -> Tuple[int, str, Optional[int]]:
    deadline = time.monotonic() + timeout_sec
    marker = "raw_feed_payload="
    while time.monotonic() < deadline:
        line = read_line_with_timeout(proc, 0.5)
        if line is None:
            continue
        sys.stdout.write(line)
        if marker not in line:
            continue

        payload_str = line.split(marker, 1)[1].strip()
        payload = json.loads(payload_str)
        messages = payload.get("messages") or []
        if not messages:
            raise RuntimeError("captured raw payload has no messages")
        selected_seq: Optional[int] = None
        selected_kind: Optional[int] = None
        for message in messages:
            seq = message.get("sequenceNumber")
            if not isinstance(seq, int):
                continue
            if minimum_sequence is not None and seq < minimum_sequence:
                continue

            kind = extract_message_kind(message)
            if required_kind is None or kind == required_kind:
                selected_seq = seq
                selected_kind = kind
                break

        if selected_seq is None:
            first_seq = messages[0].get("sequenceNumber")
            first_kind = extract_kind(payload)
            print(
                f"skipping captured payload seq={first_seq} kind={first_kind}; waiting for required kind={required_kind}"
            )
            continue
        return selected_seq, payload_str, selected_kind

    raise RuntimeError(f"timed out waiting for raw feed payload after {timeout_sec}s")


def terminate_proc(proc: subprocess.Popen) -> None:
    if proc.poll() is not None:
        return
    proc.send_signal(signal.SIGINT)
    try:
        proc.wait(timeout=3)
        return
    except subprocess.TimeoutExpired:
        pass
    proc.kill()
    proc.wait(timeout=3)


def main() -> int:
    script_dir = os.path.dirname(os.path.abspath(__file__))
    arb_revm_dir_default = os.path.dirname(script_dir)
    workspace_dir_default = os.path.dirname(arb_revm_dir_default)

    parser = argparse.ArgumentParser(
        description=(
            "Capture a live Nitro feed message, replay it via arb-revm, then verify "
            "the produced diff artifact against Nitro RPC."
        )
    )
    parser.add_argument("--rpc-url", default="http://127.0.0.1:8547")
    parser.add_argument("--feed-url", default="ws://127.0.0.1:9642")
    parser.add_argument(
        "--sequencer-client-dir",
        default=os.path.join(workspace_dir_default, "sequencer_client"),
    )
    parser.add_argument(
        "--nitro-testnode-dir",
        default=os.path.join(workspace_dir_default, "nitro-testnode"),
    )
    parser.add_argument("--arb-revm-dir", default=arb_revm_dir_default)
    parser.add_argument("--payload-out", default="/tmp/live_parity_feed_payload.json")
    parser.add_argument(
        "--artifact-out", default="/tmp/live_parity_execution_diff.json"
    )
    parser.add_argument("--ready-timeout", type=float, default=20.0)
    parser.add_argument("--capture-timeout", type=float, default=90.0)
    parser.add_argument(
        "--bridge-command",
        default="./test-node.bash script bridge-funds",
        help="Command executed in nitro-testnode dir to emit a fresh deposit/retryable message",
    )
    parser.add_argument(
        "--trigger-shell",
        action="store_true",
        help="Run --bridge-command through the shell (allows env var prefixes and compound commands)",
    )
    parser.add_argument(
        "--required-kind",
        type=int,
        default=None,
        help="Require captured feed message header kind (e.g. 12 for deposit, 9 for submit-retryable)",
    )
    parser.add_argument(
        "--feed-from-sequence",
        type=int,
        default=None,
        help="Request the feed backlog starting at this sequence number.",
    )
    parser.add_argument(
        "--minimum-sequence",
        type=int,
        default=None,
        help="Ignore feed payloads whose selected sequence is lower than this value.",
    )
    parser.add_argument(
        "--log-all-feed-payloads",
        action="store_true",
        help="Emit raw feed payloads for ordinary sequencer messages as well as deposits and retryables.",
    )
    parser.add_argument(
        "--no-bridge-trigger",
        action="store_true",
        help="Skip triggering bridge command; only capture next live message",
    )
    args = parser.parse_args()

    print("starting sequencer feed capture...")
    feed_env = os.environ.copy()
    if args.feed_from_sequence is not None:
        feed_env["ARB_FEED_FROM_SEQUENCE"] = str(args.feed_from_sequence)
    if args.log_all_feed_payloads:
        feed_env["ARB_FEED_LOG_RAW"] = "1"
    feed_proc = subprocess.Popen(
        ["cargo", "run", "-p", "sequencer_client", "--", args.feed_url],
        cwd=args.sequencer_client_dir,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        bufsize=1,
        env=feed_env,
    )

    try:
        wait_for_feed_ready(feed_proc, args.ready_timeout)

        if not args.no_bridge_trigger:
            print(f"triggering bridge command: {args.bridge_command}")
            if args.trigger_shell:
                bridge = run_cmd(
                    args.bridge_command,
                    cwd=args.nitro_testnode_dir,
                    check=True,
                    shell=True,
                )
            else:
                bridge_cmd = shlex.split(args.bridge_command)
                bridge = run_cmd(bridge_cmd, cwd=args.nitro_testnode_dir, check=True)
            print(bridge.stdout)

        seq, payload_str, kind = capture_raw_payload(
            feed_proc,
            args.capture_timeout,
            args.required_kind,
            args.minimum_sequence,
        )
        with open(args.payload_out, "w", encoding="utf-8") as handle:
            handle.write(payload_str)
            handle.write("\n")
        print(
            f"captured sequence_number={seq} kind={kind} -> {args.payload_out}"
        )
    finally:
        terminate_proc(feed_proc)

    replay_cmd = [
        "cargo",
        "run",
        "-p",
        "arb-revm",
        "--bin",
        "arb-revm",
        "--",
        args.rpc_url,
        "seq-prev",
        args.payload_out,
        "--sequence-number",
        str(seq),
        "--dump-diff",
        args.artifact_out,
    ]
    print("running arb-revm replay...")
    replay = run_cmd(replay_cmd, cwd=args.arb_revm_dir, check=True)
    print(replay.stdout)

    verify_cmd = [
        "python3",
        "scripts/verify_with_nitro.py",
        "--rpc-url",
        args.rpc_url,
        "--artifact",
        args.artifact_out,
    ]
    print("running nitro parity verifier...")
    verify = run_cmd(verify_cmd, cwd=args.arb_revm_dir, check=False)
    print(verify.stdout)

    if verify.returncode != 0:
        print(
            "live parity harness failed\n"
            f"payload: {args.payload_out}\n"
            f"artifact: {args.artifact_out}"
        )
        return verify.returncode

    print(
        "live parity harness passed\n"
        f"payload: {args.payload_out}\n"
        f"artifact: {args.artifact_out}"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
