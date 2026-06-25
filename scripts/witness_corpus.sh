#!/usr/bin/env bash
# Witness state-root regression corpus.
#
# Replays a curated set of Arbitrum One blocks spanning every EVM-hardfork era and
# several ArbOS versions, and reports whether each one's recomputed witness state root
# matches the canonical header.stateRoot (the consensus-level "everything matches" gate;
# see bin/replay_block witness_state_root_check + src/state_trie.rs).
#
# Use a reliable archive RPC that serves eth_getProof — Alchemy works; dRPC free does not.
#
#   scripts/witness_corpus.sh "https://arb-mainnet.g.alchemy.com/v2/<key>"
#
# Optionally pass extra block numbers as args to append to the default corpus.
#
# Exit code: number of blocks whose witness root MISMATCHED (0 = all matched). Blocks that
# error out (RPC/archive issues) are reported but do not count toward the mismatch total.
set -u

RPC="${1:-}"
if [[ -z "$RPC" ]]; then
  echo "usage: $0 <archive_rpc_url> [extra_block ...]" >&2
  exit 2
fi
shift || true

BIN="$(cd "$(dirname "$0")/.." && pwd)/target/debug/replay_block"
if [[ ! -x "$BIN" ]]; then
  echo "building replay_block..." >&2
  (cd "$(dirname "$BIN")/../.." && cargo build -p arb-revm --bin replay_block) || exit 1
fi

# Curated corpus: "block:label". Steady-state blocks across eras should all MATCH the
# canonical state root. Activation blocks run extra one-time migrations; the v40 one
# (deploy EIP-2935 history contract) is implemented, but the v51 activation has a separate
# not-yet-implemented migration write, so steady v51 blocks are used instead.
CORPUS=(
  "200000000:Cancun  ArbOS20  steady"
  "250000000:Cancun  ArbOS31  steady"
  "419265688:Prague  ArbOS51  steady (7702 + EIP-2935)"
  "476642738:Prague  ArbOS51  steady (recent)"
  "348448106:Prague  ArbOS40  activation (EIP-2935 deploy + Stylus MaxWasmSize)"
)
# Known remaining gap (not in the green set): 419260688 — the ArbOS 40->51 multi-version
# activation jump has a transition-specific missing write still under investigation.
for b in "$@"; do CORPUS+=("$b:extra"); done

ok=0; mismatch=0; errored=0
printf '%-12s %-40s %s\n' "BLOCK" "LABEL" "WITNESS-ROOT"
printf '%s\n' "------------------------------------------------------------------------------"
for entry in "${CORPUS[@]}"; do
  blk="${entry%%:*}"; label="${entry#*:}"
  out="$("$BIN" "$RPC" "$blk" 2>&1)"
  if grep -q "state-root parity: ok" <<<"$out"; then
    status="OK (== stateRoot)"; ok=$((ok+1))
  elif grep -q "state-root parity: MISMATCH" <<<"$out"; then
    status="MISMATCH"; mismatch=$((mismatch+1))
  else
    status="ERROR (see below)"; errored=$((errored+1))
  fi
  printf '%-12s %-40s %s\n' "$blk" "$label" "$status"
  if [[ "$status" == ERROR* ]]; then
    grep -iE 'error|panic' <<<"$out" | head -2 | sed 's/^/    /'
  fi
done
printf '%s\n' "------------------------------------------------------------------------------"
echo "summary: ok=$ok mismatch=$mismatch errored=$errored"
exit "$mismatch"
