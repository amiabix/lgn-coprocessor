use alloy_primitives::Address;
use derive_debug_plus::Dbg;
use ethers::types::H256;
use serde_derive::Deserialize;
use serde_derive::Serialize;

use super::node_type;
use super::ConcreteValueExtractionCircuitInput;
use super::NodeType;
use crate::BlockNr;
use crate::TableHash;
use crate::TableId;

pub type Identifier = u64;
pub type MptNodeVersion = (BlockNr, H256);

#[derive(Deserialize, Serialize)]
pub struct Mpt {
    pub table_hash: TableHash,
    pub block_nr: BlockNr,
    pub node_hash: H256,
    pub circuit_input: ConcreteValueExtractionCircuitInput,
}

impl Mpt {
    pub fn new(
        table_hash: TableId,
        block_nr: BlockNr,
        node_hash: H256,
        circuit_input: ConcreteValueExtractionCircuitInput,
    ) -> Self {
        Self {
            table_hash,
            block_nr,
            node_hash,
            circuit_input,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MappingLeafInput {
    pub key: Vec<u8>,
    pub node: Vec<u8>,
    pub slot: u8,
    pub key_id: u64,
    pub value_id: u64,
}

impl MappingLeafInput {
    pub fn new(
        key: Vec<u8>,
        node: Vec<u8>,
        slot: u8,
        key_id: u64,
        value_id: u64,
    ) -> Self {
        Self {
            key,
            node,
            slot,
            key_id,
            value_id,
        }
    }
}

#[derive(Dbg, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MappingBranchInput {
    pub node: Vec<u8>,

    pub children: Vec<MptNodeVersion>,

    #[dbg(placeholder = "...")]
    pub children_proofs: Vec<Vec<u8>>,
}

impl MappingBranchInput {
    pub fn new(
        node: Vec<u8>,
        children: Vec<MptNodeVersion>,
    ) -> Self {
        Self {
            node,
            children,
            children_proofs: vec![],
        }
    }
}

#[derive(Dbg, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VariableBranchInput {
    pub table_id: TableId,
    pub node: Vec<u8>,
    pub children: Vec<MptNodeVersion>,

    #[dbg(placeholder = "...")]
    pub children_proofs: Vec<Vec<u8>>,
}

impl VariableBranchInput {
    pub fn new(
        table_id: TableId,
        node: Vec<u8>,
        children: Vec<MptNodeVersion>,
    ) -> Self {
        Self {
            table_id,
            node,
            children,
            children_proofs: vec![],
        }
    }
}

#[derive(Clone, Dbg, PartialEq, Deserialize, Serialize)]
pub struct BatchedLength {
    pub table_hash: TableHash,
    pub block_nr: BlockNr,
    pub length_slot: usize,
    pub variable_slot: usize,

    #[dbg(placeholder = "...")]
    pub nodes: Vec<Vec<u8>>,
}

impl BatchedLength {
    pub fn extraction_types(&self) -> anyhow::Result<Vec<NodeType>> {
        self.nodes.iter().map(|node| node_type(node)).collect()
    }
}

#[derive(Dbg, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BatchedContract {
    pub block_nr: BlockNr,
    pub storage_root: Vec<u8>,
    pub contract: Address,

    #[dbg(placeholder = "...")]
    pub nodes: Vec<Vec<u8>>,
}

impl BatchedContract {
    pub fn extraction_types(&self) -> anyhow::Result<Vec<NodeType>> {
        self.nodes.iter().map(|node| node_type(node)).collect()
    }
}

#[derive(Clone, Dbg, PartialEq, Deserialize, Serialize)]
pub struct BlockExtractionInput {
    #[dbg(placeholder = "...")]
    pub rlp_header: Vec<u8>,
}

impl BlockExtractionInput {
    pub fn new(rlp_header: Vec<u8>) -> Self {
        Self { rlp_header }
    }
}

/// Inputs for the final extraction.
#[derive(Clone, Dbg, PartialEq, Deserialize, Serialize)]
pub enum FinalExtraction {
    Single(SingleTableExtraction),
    Merge(MergeTableExtraction),
}

impl FinalExtraction {
    pub fn new_single_table(
        table_id: TableId,
        table_hash: TableHash,
        block_nr: BlockNr,
        contract: Address,
        value_proof_version: MptNodeVersion,
    ) -> Self {
        Self::Single(SingleTableExtraction::new(
            table_id,
            table_hash,
            block_nr,
            contract,
            value_proof_version,
        ))
    }

    pub fn new_merge_table(
        table_id: TableId,
        simple_table_hash: TableHash,
        mapping_table_hash: TableHash,
        block_nr: BlockNr,
        contract: Address,
        value_proof_version: MptNodeVersion,
    ) -> Self {
        Self::Merge(MergeTableExtraction::new(
            table_id,
            simple_table_hash,
            mapping_table_hash,
            block_nr,
            contract,
            value_proof_version,
        ))
    }
}

/// Inputs for a single table proof.
///
/// # Identifiers
///
/// A [SingleTableExtraction] is either a final which binds together a block, contract, and a
/// table. The table may be either a simple, mapping, or mapping with length
#[derive(Clone, Dbg, PartialEq, Deserialize, Serialize)]
pub struct SingleTableExtraction {
    pub table_id: TableId,
    pub table_hash: TableHash,
    pub value_proof_version: MptNodeVersion,
    pub block_nr: BlockNr,
    pub contract: Address,
    pub extraction_type: FinalExtractionType,

    #[dbg(placeholder = "...")]
    pub block_proof: Vec<u8>,

    #[dbg(placeholder = "...")]
    pub contract_proof: Vec<u8>,

    #[dbg(placeholder = "...")]
    pub value_proof: Vec<u8>,

    #[dbg(placeholder = "...")]
    pub length_proof: Vec<u8>,
}

impl SingleTableExtraction {
    pub fn new(
        table_id: TableId,
        table_hash: TableHash,
        block_nr: BlockNr,
        contract: Address,
        value_proof_version: MptNodeVersion,
    ) -> Self {
        Self {
            table_id,
            table_hash,
            block_nr,
            contract,
            value_proof_version,
            extraction_type: todo!(),
            block_proof: vec![],
            contract_proof: vec![],
            value_proof: vec![],
            length_proof: vec![],
        }
    }
}

/// Inputs for a merge table proof.
///
/// # Identifiers
///
/// A [MergeTableExtraction] is a final extraction which binds together a block, contract, and its
/// two sub-tables.
#[derive(Clone, Dbg, PartialEq, Deserialize, Serialize)]
pub struct MergeTableExtraction {
    pub table_id: TableId,
    pub simple_table_hash: TableHash,
    pub mapping_table_hash: TableHash,
    pub block_nr: BlockNr,
    pub contract: Address,

    /// Determines the version of the storage node.
    ///
    /// The version is determined by the last block_nr at which the storage changed, and its hash.
    /// A single value is necessary for the simple and mapping tables because the data comes from
    /// the same contract.
    pub value_proof_version: MptNodeVersion,

    #[dbg(placeholder = "...")]
    pub block_proof: Vec<u8>,

    #[dbg(placeholder = "...")]
    pub contract_proof: Vec<u8>,

    #[dbg(placeholder = "...")]
    pub simple_table_proof: Vec<u8>,

    #[dbg(placeholder = "...")]
    pub mapping_table_proof: Vec<u8>,
}

impl MergeTableExtraction {
    pub fn new(
        table_id: TableId,
        simple_table_hash: TableHash,
        mapping_table_hash: TableHash,
        block_nr: BlockNr,
        contract: Address,
        value_proof_version: MptNodeVersion,
    ) -> Self {
        Self {
            table_id,
            simple_table_hash,
            mapping_table_hash,
            block_nr,
            contract,
            value_proof_version,
            block_proof: vec![],
            contract_proof: vec![],
            simple_table_proof: vec![],
            mapping_table_proof: vec![],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub enum FinalExtractionType {
    Simple,
    Lengthed,
}
