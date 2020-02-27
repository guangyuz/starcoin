// Copyright (c) The Starcoin Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::ensure_slice_len_eq;
use crate::storage::{CodecStorage, KeyCodec, Repository, ValueCodec};
use accumulator::node::{InternalNode, ACCUMULATOR_PLACEHOLDER_HASH};
use accumulator::node_index::NodeIndex;
use accumulator::{
    Accumulator, AccumulatorNode, AccumulatorNodeReader, AccumulatorNodeStore,
    AccumulatorNodeWriter, AccumulatorProof, LeafCount, MerkleAccumulator,
};
use anyhow::Error;
use anyhow::{bail, ensure, format_err, Result};
use byteorder::{BigEndian, ReadBytesExt};
use crypto::hash::HashValue;
use scs::SCSCodec;
use std::collections::HashMap;
use std::hash::Hash;
use std::marker::PhantomData;
use std::mem::size_of;
use std::sync::Arc;

pub type MockAccumulator<'a> = MerkleAccumulator<'a, MockHashStore>;

pub struct MockHashStore {
    index_storage: CodecStorage<NodeIndex, HashValue>,
    node_store: CodecStorage<HashValue, AccumulatorNode>,
}

const MOCK_ACCUMULATOR_INDEX_KEY_PREFIX: &str = "NockAccumulatorIndex";
const MOCK_ACCUMULATOR_NODE_KEY_PREFIX: &str = "NockAccumulatorNode";

impl MockHashStore {
    pub fn new(storage: Arc<dyn Repository>) -> Self {
        Self {
            index_storage: CodecStorage::new(storage.clone(), MOCK_ACCUMULATOR_INDEX_KEY_PREFIX),
            node_store: CodecStorage::new(storage.clone(), MOCK_ACCUMULATOR_NODE_KEY_PREFIX),
        }
    }
}

impl KeyCodec for NodeIndex {
    fn encode_key(&self) -> Result<Vec<u8>> {
        Ok(self.to_inorder_index().to_be_bytes().to_vec())
    }

    fn decode_key(data: &[u8]) -> Result<Self> {
        ensure_slice_len_eq(data, size_of::<u64>())?;
        let index = (&data[..]).read_u64::<BigEndian>()?;
        Ok(NodeIndex::new(index))
    }
}

impl ValueCodec for AccumulatorNode {
    fn encode_value(&self) -> Result<Vec<u8>> {
        self.encode()
    }

    fn decode_value(data: &[u8]) -> Result<Self> {
        Self::decode(data)
    }
}

impl AccumulatorNodeStore for MockHashStore {}
impl AccumulatorNodeReader for MockHashStore {
    fn get(&self, index: NodeIndex) -> Result<Option<AccumulatorNode>, Error> {
        let node_index = self.index_storage.get(index).unwrap();
        match node_index {
            Some(hash) => self.node_store.get(hash),
            None => bail!("get accumulator node index is null {:?}", node_index),
        }
    }

    fn get_node(&self, hash: HashValue) -> Result<Option<AccumulatorNode>> {
        self.node_store.get(hash)
    }
}

impl AccumulatorNodeWriter for MockHashStore {
    fn save(&self, index: NodeIndex, hash: HashValue) -> Result<(), Error> {
        self.index_storage.put(index, hash)
    }

    fn save_node(&self, node: AccumulatorNode) -> Result<()> {
        self.node_store.put(node.hash(), node)
    }
}