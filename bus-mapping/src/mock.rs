//! Mock types and functions to generate mock data useful for tests

use crate::circuit_input_builder::{AccessSet, Block, Blocks, CircuitInputBuilder, CircuitsParams};
use eth_types::{
    geth_types::GethData,
    state_db::{self, CodeDB, StateDB},
    ToWord, Word, H256,
};
use ethers_core::utils::keccak256;

const MOCK_OLD_STATE_ROOT: u64 = 0xcafeu64;

/// BlockData is a type that contains all the information from a block required
/// to build the circuit inputs.
#[derive(Debug)]
pub struct BlockData {
    /// StateDB
    pub sdb: StateDB,
    /// CodeDB
    pub code_db: CodeDB,
    /// chain id
    pub chain_id: u64,
    /// history hashes contains most recent 256 block hashes in history, where
    /// the lastest one is at history_hashes[history_hashes.len() - 1].
    pub history_hashes: Vec<Word>,
    /// Block from geth
    pub eth_block: eth_types::Block<eth_types::Transaction>,
    /// Execution Trace from geth
    pub geth_traces: Vec<eth_types::GethExecTrace>,
    /// Circuits setup parameters
    pub circuits_params: CircuitsParams,
}

impl BlockData {
    /// Generate a new CircuitInputBuilder initialized with the context of the
    /// BlockData.
    pub fn new_circuit_input_builder(&self) -> CircuitInputBuilder {
        let header =
            Block::new(self.chain_id, self.history_hashes.clone(), &self.eth_block).unwrap();
        let mut block = Blocks::init(self.chain_id, Default::default());
        block.blocks.insert(header.number.as_u64(), header);
        // FIXME: better fetch a real state root instead of a mock one
        block.prev_state_root = MOCK_OLD_STATE_ROOT.into();
        block.circuits_params = self.circuits_params;
        CircuitInputBuilder::new(self.sdb.clone(), self.code_db.clone(), &block)
    }
    /// Create a new block from the given Geth data.
    pub fn new_from_geth_data_with_params(
        geth_data: GethData,
        circuits_params: CircuitsParams,
    ) -> Self {
        let mut sdb = StateDB::new();
        let mut code_db = CodeDB::new();

        let access_set = AccessSet::from_geth_data(&geth_data);
        // Initialize all accesses accounts to zero
        for addr in access_set.state.keys() {
            sdb.set_account(addr, state_db::Account::zero());
        }

        for account in geth_data.accounts {
            let keccak_code_hash = H256(keccak256(&account.code));
            log::trace!(
                "trace code {:?} {:?}",
                keccak_code_hash,
                hex::encode(&account.code)
            );
            let code_hash = code_db.insert(account.code.to_vec());
            sdb.set_account(
                &account.address,
                state_db::Account {
                    nonce: account.nonce,
                    balance: account.balance,
                    storage: account.storage,
                    code_hash,
                    keccak_code_hash,
                    code_size: account.code.len().to_word(),
                },
            );
        }

        Self {
            sdb,
            code_db,
            chain_id: geth_data.chain_id,
            history_hashes: geth_data.history_hashes,
            eth_block: geth_data.eth_block,
            geth_traces: geth_data.geth_traces,
            circuits_params,
        }
    }

    /// Create a new block from the given Geth data with default CircuitsParams.
    pub fn new_from_geth_data(geth_data: GethData) -> Self {
        Self::new_from_geth_data_with_params(geth_data, CircuitsParams::default())
    }
}

#[cfg(test)]
#[ctor::ctor]
fn init_env_logger() {
    // Enable RUST_LOG during tests
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("error")).init();
}
