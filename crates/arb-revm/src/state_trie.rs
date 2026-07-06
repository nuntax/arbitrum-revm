//! Witness-based Merkle-Patricia-Trie root recomputation for state-root parity checks.
//!
//! Given a set of Merkle-proof nodes (from `eth_getProof`) anchored to a known trie
//! root, plus a set of updates, this recomputes the resulting root. Untouched subtrees
//! are carried by their committed hash (via `HashBuilder::add_branch`), so a change that
//! *should* have happened but didn't surfaces as a root mismatch, the blind spot that
//! the per-write proof check (`verify_writes_against_state_root`) structurally cannot see.
//!
//! Both the account trie and each storage trie are the same secure-MPT shape (32-byte
//! keccak keys), so one routine serves both; only the leaf value differs (RLP of a
//! `TrieAccount` vs RLP of a `U256` storage word), which the caller encodes.

use std::collections::{BTreeMap, HashMap};

use alloy_rlp::Decodable;
use alloy_trie::{
    EMPTY_ROOT_HASH, HashBuilder, Nibbles,
    nodes::{RlpNode, TrieNode},
};
use revm::primitives::{B256, Bytes, keccak256};

/// A reconstructed entry in the partial trie, keyed by full nibble path.
enum Entry {
    /// A leaf with its RLP-encoded value.
    Leaf(Vec<u8>),
    /// An opaque subtree known only by its hash (contains no proven/updated key).
    Subtree(B256),
}

/// Recompute an MPT root from proof nodes plus a set of updates.
///
/// - `root`: the trie root the `proof_nodes` are anchored to.
/// - `proof_nodes`: all proof-node RLPs (an account proof, or the union of an account's
///   per-slot storage proofs).
/// - `updates`: full keccak-key nibble paths → `Some(rlp_value)` to set/insert, or `None`
///   to delete. Keys must be the unpacked nibbles of `keccak256(key_bytes)`.
///
/// Returns the recomputed root. With an empty `updates` map this reproduces `root`
/// exactly (an identity/self-test of the reconstruction); with updates applied it yields
/// the post-update root to compare against the canonical one.
pub fn recompute_root(
    root: B256,
    proof_nodes: &[Bytes],
    updates: &BTreeMap<Nibbles, Option<Vec<u8>>>,
) -> Result<B256, String> {
    let mut by_hash: HashMap<B256, &[u8]> = HashMap::with_capacity(proof_nodes.len());
    for n in proof_nodes {
        by_hash.insert(keccak256(n.as_ref()), n.as_ref());
    }

    let mut entries: BTreeMap<Nibbles, Entry> = BTreeMap::new();
    if root != EMPTY_ROOT_HASH {
        let root_bytes = by_hash
            .get(&root)
            .copied()
            .ok_or_else(|| format!("root node {root:#x} not present in proof set"))?;
        collect(root_bytes, Nibbles::new(), &by_hash, &mut entries)?;
    }

    // Apply updates over the revealed entries. A proven key always lands in the revealed
    // region (we fetched its proof), so it never falls inside an opaque subtree.
    for (key, val) in updates {
        match val {
            Some(rlp) => {
                entries.insert(*key, Entry::Leaf(rlp.clone()));
            }
            None => {
                entries.remove(key);
            }
        }
    }

    // Feed HashBuilder in ascending nibble order (BTreeMap order == Nibbles::Ord, which is
    // exactly what HashBuilder requires). HashBuilder rebuilds all trie structure itself.
    let mut hb = HashBuilder::default();
    for (key, entry) in &entries {
        match entry {
            Entry::Leaf(v) => hb.add_leaf(*key, v),
            Entry::Subtree(h) => hb.add_branch(*key, *h, false),
        }
    }
    Ok(hb.root())
}

/// Walk a revealed node, recording its leaves and opaque-subtree boundaries.
fn collect(
    node_bytes: &[u8],
    path: Nibbles,
    by_hash: &HashMap<B256, &[u8]>,
    entries: &mut BTreeMap<Nibbles, Entry>,
) -> Result<(), String> {
    let node =
        TrieNode::decode(&mut &node_bytes[..]).map_err(|e| format!("trie node decode: {e}"))?;
    match node {
        TrieNode::EmptyRoot => {}
        TrieNode::Leaf(leaf) => {
            entries.insert(path.join(&leaf.key), Entry::Leaf(leaf.value));
        }
        TrieNode::Extension(ext) => {
            resolve_child(&ext.child, path.join(&ext.key), by_hash, entries)?;
        }
        TrieNode::Branch(branch) => {
            let mut stack = branch.stack.iter();
            for i in 0..16u8 {
                if branch.state_mask.is_bit_set(i) {
                    let child = stack.next().ok_or("branch stack underflow")?;
                    let mut child_path = path;
                    child_path.push(i);
                    resolve_child(child, child_path, by_hash, entries)?;
                }
            }
        }
    }
    Ok(())
}

/// Resolve a branch/extension child reference: recurse if revealed in the proof set,
/// otherwise record it as an opaque subtree carried by its committed hash.
fn resolve_child(
    child: &RlpNode,
    path: Nibbles,
    by_hash: &HashMap<B256, &[u8]>,
    entries: &mut BTreeMap<Nibbles, Entry>,
) -> Result<(), String> {
    if let Some(hash) = child.as_hash() {
        match by_hash.get(&hash) {
            Some(bytes) => collect(bytes, path, by_hash, entries)?,
            None => {
                entries.insert(path, Entry::Subtree(hash));
            }
        }
    } else {
        // Inline node (<32 bytes): the RlpNode slice is the child node's RLP directly.
        collect(child.as_slice(), path, by_hash, entries)?;
    }
    Ok(())
}
