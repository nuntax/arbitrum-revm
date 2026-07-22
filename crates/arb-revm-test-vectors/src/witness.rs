//! Strict execution-witness database.
//!
//! The importer treats Nitro witness bytes as opaque MPT nodes. It walks the
//! authenticated trie, so a missing node is an input failure instead of a zero
//! account or storage slot.

use std::collections::{BTreeMap, BTreeSet};

use alloy_consensus::Header;
use alloy_rlp::Decodable;
use alloy_trie::{
    Nibbles, TrieAccount,
    nodes::{RlpNode, TrieNode},
};
use revm::{
    DatabaseRef,
    primitives::{Address, B256, Bytes, KECCAK_EMPTY, StorageKey, StorageValue, U256, keccak256},
    state::{AccountInfo, Bytecode},
};
use serde::Deserialize;

use crate::strict_db::IncompleteWitness;

const WITNESS_SCHEMA: &str = "arb-stf-execution-witness-v1";

/// Serializable form written by the capture process.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionWitnessPrestate {
    pub schema: String,
    pub parent_header_rlp: Bytes,
    pub raw_headers: Vec<RawHeader>,
    pub witness: RpcWitness,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawHeader {
    pub number: u64,
    pub rlp: Bytes,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RpcWitness {
    pub headers: Vec<serde_json::Value>,
    pub codes: Vec<Bytes>,
    pub state: Vec<Bytes>,
    pub keys: Vec<Bytes>,
}

/// Parent-state reader reconstructed directly from an execution witness.
#[derive(Debug, Clone)]
pub struct WitnessDatabase {
    state_root: B256,
    nodes: BTreeMap<B256, Bytes>,
    codes: BTreeMap<B256, Bytecode>,
    block_hashes: BTreeMap<u64, B256>,
}

impl WitnessDatabase {
    pub fn from_prestate(prestate: ExecutionWitnessPrestate) -> Result<Self, IncompleteWitness> {
        if prestate.schema != WITNESS_SCHEMA {
            return Err(IncompleteWitness::InvalidCompleteState(format!(
                "unsupported execution-witness schema {:?}",
                prestate.schema
            )));
        }
        let parent = decode_header(&prestate.parent_header_rlp)?;
        let mut nodes = BTreeMap::new();
        for node in prestate.witness.state {
            let hash = keccak256(&node);
            if nodes.insert(hash, node).is_some() {
                return Err(IncompleteWitness::InvalidCompleteState(format!(
                    "duplicate trie node {hash:#x}"
                )));
            }
        }
        if parent.state_root != alloy_trie::EMPTY_ROOT_HASH
            && !nodes.contains_key(&parent.state_root)
        {
            return Err(IncompleteWitness::InvalidCompleteState(format!(
                "parent state-root node {:#x} is absent from witness",
                parent.state_root
            )));
        }
        let mut codes = BTreeMap::new();
        codes.insert(KECCAK_EMPTY, Bytecode::default());
        for code in prestate.witness.codes {
            let hash = keccak256(&code);
            if codes.insert(hash, Bytecode::new_raw(code)).is_some() {
                return Err(IncompleteWitness::InvalidCompleteState(format!(
                    "duplicate code blob {hash:#x}"
                )));
            }
        }
        let mut block_hashes = BTreeMap::new();
        let mut seen_numbers = BTreeSet::new();
        for raw in prestate.raw_headers {
            if !seen_numbers.insert(raw.number) {
                return Err(IncompleteWitness::InvalidCompleteState(format!(
                    "duplicate raw header at {}",
                    raw.number
                )));
            }
            let header = decode_header(&raw.rlp)?;
            if header.number != raw.number {
                return Err(IncompleteWitness::InvalidCompleteState(format!(
                    "raw header number mismatch: index {}, RLP {}",
                    raw.number, header.number
                )));
            }
            block_hashes.insert(raw.number, header.hash_slow());
        }
        block_hashes
            .entry(parent.number)
            .or_insert_with(|| parent.hash_slow());
        Ok(Self {
            state_root: parent.state_root,
            nodes,
            codes,
            block_hashes,
        })
    }

    pub fn state_root(&self) -> B256 {
        self.state_root
    }

    pub fn trie_nodes(&self) -> Vec<Bytes> {
        self.nodes.values().cloned().collect()
    }

    pub fn trie_account(&self, address: Address) -> Result<Option<TrieAccount>, IncompleteWitness> {
        let key = keccak256(address);
        let Some(value) = self.trie_value(self.state_root, key)? else {
            return Ok(None);
        };
        TrieAccount::decode(&mut value.as_ref())
            .map(Some)
            .map_err(|error| {
                IncompleteWitness::InvalidCompleteState(format!(
                    "account leaf decode for {address:#x}: {error}"
                ))
            })
    }

    fn trie_value(&self, root: B256, key: B256) -> Result<Option<Vec<u8>>, IncompleteWitness> {
        if root == alloy_trie::EMPTY_ROOT_HASH {
            return Ok(None);
        }
        let mut current = self.node_by_hash(root)?;
        let key = Nibbles::unpack(key);
        let mut position = 0;
        loop {
            let node = TrieNode::decode(&mut current.as_ref()).map_err(|error| {
                IncompleteWitness::InvalidCompleteState(format!(
                    "witness trie node decode: {error}"
                ))
            })?;
            match node {
                TrieNode::EmptyRoot => return Ok(None),
                TrieNode::Leaf(leaf) => {
                    return Ok((key.slice(position..) == leaf.key).then_some(leaf.value));
                }
                TrieNode::Extension(extension) => {
                    let suffix = key.slice(position..);
                    if !suffix.starts_with(&extension.key) {
                        return Ok(None);
                    }
                    position += extension.key.len();
                    current = self.resolve_child(&extension.child)?;
                }
                TrieNode::Branch(branch) => {
                    let Some(nibble) = key.get(position) else {
                        return Ok(None);
                    };
                    let branch = branch.as_ref();
                    let child = branch
                        .children()
                        .find_map(|(index, child)| (index == nibble).then_some(child).flatten());
                    let Some(child) = child else {
                        return Ok(None);
                    };
                    position += 1;
                    current = self.resolve_child(child)?;
                }
            }
        }
    }

    fn node_by_hash(&self, hash: B256) -> Result<Bytes, IncompleteWitness> {
        self.nodes.get(&hash).cloned().ok_or_else(|| {
            IncompleteWitness::InvalidCompleteState(format!(
                "incomplete witness: trie node {hash:#x}"
            ))
        })
    }

    fn resolve_child(&self, child: &RlpNode) -> Result<Bytes, IncompleteWitness> {
        match child.as_hash() {
            Some(hash) => self.node_by_hash(hash),
            None => Ok(Bytes::copy_from_slice(child.as_slice())),
        }
    }
}

impl DatabaseRef for WitnessDatabase {
    type Error = IncompleteWitness;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let Some(account) = self.trie_account(address)? else {
            return Ok(None);
        };
        let code = self
            .codes
            .get(&account.code_hash)
            .cloned()
            .ok_or(IncompleteWitness::Code {
                code_hash: account.code_hash,
            })?;
        Ok(Some(AccountInfo::new(
            account.balance,
            account.nonce,
            account.code_hash,
            code,
        )))
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.codes
            .get(&code_hash)
            .cloned()
            .ok_or(IncompleteWitness::Code { code_hash })
    }

    fn storage_ref(
        &self,
        address: Address,
        index: StorageKey,
    ) -> Result<StorageValue, Self::Error> {
        let Some(account) = self.trie_account(address)? else {
            return Ok(U256::ZERO);
        };
        let key = keccak256(B256::from(index.to_be_bytes::<32>()));
        let Some(value) = self.trie_value(account.storage_root, key)? else {
            return Ok(U256::ZERO);
        };
        U256::decode(&mut value.as_ref()).map_err(|error| {
            IncompleteWitness::InvalidCompleteState(format!(
                "storage leaf decode {address:#x}[{index:#x}]: {error}"
            ))
        })
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        self.block_hashes
            .get(&number)
            .copied()
            .ok_or(IncompleteWitness::BlockHash { number })
    }
}

fn decode_header(rlp: &Bytes) -> Result<Header, IncompleteWitness> {
    Header::decode(&mut rlp.as_ref()).map_err(|error| {
        IncompleteWitness::InvalidCompleteState(format!("witness header decode: {error}"))
    })
}

#[cfg(test)]
mod tests {
    use alloy_rlp::Encodable;
    use alloy_trie::nodes::LeafNode;

    use super::*;

    #[test]
    fn empty_witness_proves_absence() {
        let header = Header::default();
        let mut rlp = Vec::new();
        header.encode(&mut rlp);
        let witness = ExecutionWitnessPrestate {
            schema: WITNESS_SCHEMA.to_owned(),
            parent_header_rlp: Bytes::from(rlp),
            raw_headers: Vec::new(),
            witness: RpcWitness {
                headers: Vec::new(),
                codes: Vec::new(),
                state: Vec::new(),
                keys: Vec::new(),
            },
        };
        let db = WitnessDatabase::from_prestate(witness).unwrap();
        assert_eq!(db.basic_ref(Address::ZERO).unwrap(), None);
        assert_eq!(
            db.storage_ref(Address::ZERO, U256::ZERO).unwrap(),
            U256::ZERO
        );
    }

    #[test]
    fn reads_an_account_from_a_witness_trie_node() {
        let address = Address::with_last_byte(7);
        let account = TrieAccount {
            nonce: 3,
            balance: U256::from(9),
            storage_root: alloy_trie::EMPTY_ROOT_HASH,
            code_hash: KECCAK_EMPTY,
        };
        let leaf = LeafNode::new(
            Nibbles::unpack(keccak256(address)),
            alloy_rlp::encode(account),
        );
        let mut node = Vec::new();
        leaf.encode(&mut node);
        let header = Header {
            state_root: keccak256(&node),
            ..Default::default()
        };
        let mut rlp = Vec::new();
        header.encode(&mut rlp);
        let witness = ExecutionWitnessPrestate {
            schema: WITNESS_SCHEMA.to_owned(),
            parent_header_rlp: Bytes::from(rlp),
            raw_headers: Vec::new(),
            witness: RpcWitness {
                headers: Vec::new(),
                codes: Vec::new(),
                state: vec![Bytes::from(node)],
                keys: Vec::new(),
            },
        };
        let db = WitnessDatabase::from_prestate(witness).unwrap();
        let info = db.basic_ref(address).unwrap().unwrap();
        assert_eq!(info.nonce, 3);
        assert_eq!(info.balance, U256::from(9));
        assert_eq!(db.basic_ref(Address::ZERO).unwrap(), None);
    }
}
