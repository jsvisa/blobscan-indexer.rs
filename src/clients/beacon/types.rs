use ethers::types::{Bytes, H256};
use serde::Deserialize;

pub enum Topic {
    ChainReorg,
}

impl Topic {
    pub fn to_string(&self) -> String {
        match self {
            Topic::ChainReorg => String::from("chain_reorg"),
        }
    }
}

#[derive(Deserialize, Debug)]
pub struct ExecutionPayload {
    pub block_hash: H256,
}

#[derive(Deserialize, Debug)]
pub struct BlockBody {
    pub execution_payload: Option<ExecutionPayload>,
    pub blob_kzg_commitments: Option<Vec<String>>,
}
#[derive(Deserialize, Debug)]
pub struct BlockMessage {
    pub slot: String,
    pub body: BlockBody,
}

#[derive(Deserialize, Debug)]
pub struct Block {
    pub message: BlockMessage,
}

#[derive(Deserialize, Debug)]
pub struct BlockResponse {
    pub data: Block,
}

#[derive(Deserialize, Debug)]
pub struct Blob {
    pub index: String,
    pub kzg_commitment: String,
    pub blob: Bytes,
}

#[derive(Deserialize, Debug)]
pub struct BlobsResponse {
    pub data: Vec<Blob>,
}

#[derive(Deserialize, Debug)]
pub struct ChainReorgResponse {
    pub slot: String,
    pub depth: String,
    pub old_head_block: String,
    pub new_head_block: String,
}
