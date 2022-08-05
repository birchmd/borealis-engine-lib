use crate::metrics::{record_metric, LATEST_BLOCK_PROCESSED};
use crate::utils::{as_h256, keccak256, TxMetadata};
use aurora_engine::parameters::{CallArgs, ResultLog, SubmitResult};
use aurora_engine_sdk::sha256;
use aurora_engine_sdk::types::near_account_to_evm_address;
use aurora_engine_transactions::{
    EthTransactionKind, NormalizedEthTransaction, ParseTransactionError,
};
use aurora_engine_types::types::{Wei, WeiU256};
use aurora_engine_types::{H256, U256};
use aurora_refiner_types::aurora_block::{
    AuroraBlock, AuroraTransaction, AuroraTransactionBuilder, AuroraTransactionBuilderError,
    NearBlock, NearBlockHeader, NearTransaction,
};
use aurora_refiner_types::bloom::Bloom;
use aurora_refiner_types::near_block::{BlockView, ExecutionOutcomeWithReceipt, NEARBlock};
use aurora_refiner_types::near_primitives::hash::CryptoHash;
use aurora_refiner_types::near_primitives::types::BlockHeight;
use aurora_refiner_types::near_primitives::views::{
    ActionView, ExecutionStatusView, ReceiptEnumView,
};
use aurora_standalone_engine::types::InnerTransactionKind;
use borsh::BorshDeserialize;
use borsh_0_9_3::BorshSerialize;
use byteorder::{BigEndian, WriteBytesExt};
use engine_standalone_storage::sync::{TransactionExecutionResult, TransactionIncludedOutcome};
use std::collections::{HashMap, HashSet};
use std::convert::{TryFrom, TryInto};
use std::io::Write;
use std::str::FromStr;
use triehash_ethereum::ordered_trie_root;

fn compute_block_hash_preimage(height: BlockHeight, chain_id: u64) -> Vec<u8> {
    let account_id = "aurora";

    let mut buffer = Vec::with_capacity(25 + 8 + account_id.len() + 8);
    let _ = buffer.write(&[0; 25]);
    let _ = buffer.write_u64::<BigEndian>(chain_id);
    let _ = buffer.write(account_id.as_bytes());
    let _ = buffer.write_u64::<BigEndian>(height);

    buffer
}

fn compute_block_hash(height: BlockHeight, chain_id: u64) -> H256 {
    sha256(&compute_block_hash_preimage(height, chain_id))
}

struct TxExtraData {
    transaction_hash: H256,
    /// Hash of the result of the transaction
    receipt_hash: H256,
}

pub struct Refiner {
    chain_id: u64,
    /// Constant value of an empty merkle tree root
    empty_merkle_tree_root: H256,
    /// Last prev_state_root known (useful to compute state roots on skip blocks)
    /// Refiner can't start from a skip block so this field will be always set.
    /// Previous valid prev_state_root will be used for next
    prev_state_root: H256,
    /// Partial state used during the computation of a block.
    partial_state: PartialState,
}

/// Data that must be recomputed on every block
#[derive(Default)]
struct PartialState {
    /// Amount of gas spent on a block
    total_gas: u64,
    /// Estimate of the size (in bytes) of all the transactions in a block
    size: u64,
    /// Partial bloom filter
    bloom_filter: Bloom,
    /// List of all current transactions
    transactions: Vec<AuroraTransaction>,
    /// Transactions data used to build transactions and receipts merkle tree
    transactions_extra_data: Vec<TxExtraData>,
    /// List with all observed receipts. A Receipt can be seen multiple times, one per action
    seen_receipts: HashSet<CryptoHash>,
}

/// Data computed by the refiner and passed to the callback
#[derive(Debug)]
pub struct RefinerItem {
    pub block: AuroraBlock,
}

impl Refiner {
    pub fn new(chain_id: u64) -> Self {
        Self {
            chain_id,
            empty_merkle_tree_root: H256::from(
                TryInto::<&[u8; 32]>::try_into(
                    &hex::decode(
                        "56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
                    )
                    .unwrap()[..],
                )
                .unwrap(),
            ),
            prev_state_root: H256::zero(),
            partial_state: Default::default(),
        }
    }
}

impl Refiner {
    pub fn on_block_skip(&mut self, height: u64, next_block: &NEARBlock) -> AuroraBlock {
        AuroraBlock {
            chain_id: self.chain_id,
            hash: compute_block_hash(height, self.chain_id),
            parent_hash: compute_block_hash(height - 1, self.chain_id),
            height,
            miner: near_account_to_evm_address(b""),
            timestamp: next_block.block.header.timestamp,
            gas_limit: U256::max_value(),
            size: U256::zero(),
            gas_used: U256::zero(),
            transactions_root: self.empty_merkle_tree_root,
            receipts_root: self.empty_merkle_tree_root,
            transactions: vec![],
            near_metadata: NearBlock::SkipBlock,
            state_root: self.prev_state_root,
            logs_bloom: Default::default(),
        }
    }

    pub fn on_block_start(&mut self, block: &NEARBlock) {
        let NEARBlock { block, shards, .. } = &block;
        // Check if all chunks were parsed
        tracing::trace!(target: "block", "Processing block at height {}, hash={}", block.header.height, block.header.hash);
        if !block.header.chunk_mask.is_empty() && block.header.chunk_mask.len() != shards.len() {
            tracing::warn!(target: "block", "Not all shards are being tracked. Expected number of shards {}, found {}", block.header.chunk_mask.len(), shards.len());
            crate::metrics::MISSING_SHARDS.inc();
        }
    }

    pub fn on_execution_outcome(
        &mut self,
        block: &NEARBlock,
        execution_outcome: &ExecutionOutcomeWithReceipt,
        txs: &HashMap<H256, TransactionIncludedOutcome>,
    ) {
        let NEARBlock { block, .. } = &block;

        if self
            .partial_state
            .seen_receipts
            .insert(execution_outcome.receipt.receipt_id)
        {
            // Using recent version of borsh to serialize the receipt.
            // Include in the size of the block the size of this transaction.
            self.partial_state.size +=
                borsh_0_9_3::BorshSerialize::try_to_vec(&execution_outcome.receipt)
                    .unwrap()
                    .len() as u64;
        }

        match &execution_outcome.receipt.receipt {
            ReceiptEnumView::Action { actions, .. } => {
                crate::metrics::TRANSACTIONS.inc();

                // Create one transaction per action
                for (index, action) in actions.iter().enumerate() {
                    crate::metrics::TRANSACTIONS_ACTION.inc();

                    let virtual_receipt_id = build_virtual_receipt_id(
                        &execution_outcome.receipt.receipt_id,
                        index as u32,
                        actions.len() as u32,
                    );

                    match build_transaction(
                        block,
                        index,
                        action,
                        execution_outcome,
                        self.chain_id,
                        self.partial_state.transactions.len() as u32,
                        virtual_receipt_id,
                        txs,
                    ) {
                        Ok(tx) => {
                            let BuiltTransaction {
                                transaction,
                                transaction_hash,
                            } = tx;

                            let result_hash = sha256(transaction.output.as_slice());
                            tracing::trace!(target: "transactions", "New transaction: {}", transaction.hash);
                            self.partial_state.total_gas += transaction.gas_used;
                            self.partial_state
                                .bloom_filter
                                .accrue_bloom(&transaction.logs_bloom);
                            self.partial_state.transactions.push(transaction);
                            self.partial_state
                                .transactions_extra_data
                                .push(TxExtraData {
                                    transaction_hash,
                                    receipt_hash: result_hash,
                                });
                        }
                        Err(err) => {
                            tracing::error!(target: "transactions", "Error while building transaction: {:?}. Block: {}", err, block.header.hash);
                            crate::metrics::ERROR_BUILDING_TRANSACTION.inc();
                        }
                    }
                }
            }
            // Ignore receipts of type Data
            ReceiptEnumView::Data { data_id, .. } => {
                crate::metrics::TRANSACTIONS_DATA.inc();
                tracing::warn!(target: "transactions",
                    "Ignore receipt data. Receipt Id: {} Data Id: {:?}",
                    execution_outcome.receipt.receipt_id,
                    data_id,
                )
            }
        }
    }

    pub fn on_block_end(&mut self, block: &NEARBlock) -> AuroraBlock {
        let NEARBlock { block, .. } = &block;

        // Compute near metadata
        let near_header = NearBlockHeader {
            near_hash: block.header.hash,
            near_parent_hash: block.header.prev_hash,
            author: block.author.clone(),
        };

        // Build transactions root
        let transactions_root = as_h256(
            ordered_trie_root(
                self.partial_state
                    .transactions_extra_data
                    .iter()
                    .map(|tx| tx.transaction_hash),
            )
            .as_bytes(),
        );
        let receipts_root = as_h256(
            ordered_trie_root(
                self.partial_state
                    .transactions_extra_data
                    .iter()
                    .map(|tx| tx.receipt_hash),
            )
            .as_bytes(),
        );

        self.prev_state_root = H256::from(block.header.prev_state_root.0);

        let aurora_block = AuroraBlock {
            chain_id: self.chain_id,
            hash: compute_block_hash(block.header.height, self.chain_id),
            parent_hash: compute_block_hash(block.header.height - 1, self.chain_id),
            height: block.header.height,
            miner: near_account_to_evm_address(block.author.as_bytes()),
            timestamp: block.header.timestamp,
            gas_limit: U256::max_value(),
            state_root: self.prev_state_root,
            size: U256::from(self.partial_state.size),
            gas_used: U256::from(self.partial_state.total_gas),
            transactions_root,
            receipts_root,
            transactions: self.partial_state.transactions.drain(..).collect(),
            near_metadata: NearBlock::ExistingBlock(near_header),
            logs_bloom: self.partial_state.bloom_filter,
        };

        LATEST_BLOCK_PROCESSED.set(block.header.height as i64);

        // Reset the partial state
        self.partial_state = Default::default();

        aurora_block
    }
}

/// Receipt id of internal actions, when several actions are batched in the same receipt
fn build_virtual_receipt_id(
    receipt_id: &CryptoHash,
    action_index: u32,
    total_actions: u32,
) -> CryptoHash {
    if action_index + 1 == total_actions {
        *receipt_id
    } else {
        let mut bytes = [0u8; 36];
        bytes[0..32].copy_from_slice(receipt_id.0.as_slice());
        bytes[32..36].copy_from_slice(&action_index.to_be_bytes());
        CryptoHash(aurora_refiner_types::utils::keccak256(&bytes).0)
    }
}

struct BuiltTransaction {
    transaction: AuroraTransaction,
    transaction_hash: H256,
}

#[allow(clippy::too_many_arguments)]
fn build_transaction(
    near_block: &BlockView,
    action_index: usize,
    action: &ActionView,
    outcome: &ExecutionOutcomeWithReceipt,
    chain_id: u64,
    transaction_index: u32,
    virtual_receipt_id: CryptoHash,
    txs: &HashMap<H256, TransactionIncludedOutcome>,
) -> Result<BuiltTransaction, RefinerError> {
    let mut bloom = Bloom::default();

    let hash;

    let mut tx = AuroraTransactionBuilder::default()
        .block_hash(compute_block_hash(near_block.header.height, chain_id))
        .block_height(near_block.header.height)
        .chain_id(chain_id)
        .transaction_index(transaction_index)
        .gas_price(U256::zero())
        .near_metadata(NearTransaction {
            action_index,
            receipt_hash: outcome.receipt.receipt_id,
        });

    // Hash used to build transactions merkle tree
    let mut transaction_hash = H256::zero();

    match action {
        ActionView::FunctionCall {
            method_name, args, ..
        } => {
            let bytes = base64::decode(&args).map_err(RefinerError::FunctionCallBase64Args)?;

            transaction_hash = sha256(bytes.as_slice());

            let raw_tx_kind: InnerTransactionKind =
                InnerTransactionKind::from_str(method_name.as_str())
                    .unwrap_or(InnerTransactionKind::Unknown);

            record_metric(&raw_tx_kind);

            if let InnerTransactionKind::Unknown = raw_tx_kind {
                tracing::warn!("Unknown method: {}", method_name);
            }

            tx = match raw_tx_kind {
                InnerTransactionKind::Submit => {
                    let tx_metadata = TxMetadata::try_from(bytes.as_slice())
                        .map_err(RefinerError::ParseMetadata)?;

                    let eth_tx: NormalizedEthTransaction =
                        EthTransactionKind::try_from(bytes.as_slice())
                            .map_err(RefinerError::ParseTransaction)?
                            .into();

                    hash = keccak256(bytes.as_slice()); // https://ethereum.stackexchange.com/a/46579/45323
                    tx = tx
                        .hash(hash)
                        .from(eth_tx.address.unwrap_or_else(|| {
                            near_account_to_evm_address(outcome.receipt.predecessor_id.as_bytes())
                        }))
                        .to(eth_tx.to)
                        .nonce(eth_tx.nonce)
                        .gas_limit(eth_tx.gas_limit)
                        .gas_price(eth_tx.max_fee_per_gas)
                        .max_priority_fee_per_gas(eth_tx.max_priority_fee_per_gas)
                        .max_fee_per_gas(eth_tx.max_fee_per_gas)
                        .value(eth_tx.value)
                        .input(eth_tx.data)
                        .access_list(eth_tx.access_list)
                        .tx_type(tx_metadata.tx_type)
                        .contract_address(None)
                        .v(tx_metadata.v)
                        .r(tx_metadata.r)
                        .s(tx_metadata.s);

                    fill_result(
                        tx,
                        &outcome.execution_outcome.outcome.status,
                        true,
                        &mut bloom,
                        txs.get(&hash),
                    )?
                }
                InnerTransactionKind::Call => {
                    hash = virtual_receipt_id.0.try_into().unwrap();
                    tx = tx.hash(hash).from(near_account_to_evm_address(
                        outcome.receipt.predecessor_id.as_bytes(),
                    ));

                    if let Some(call_args) = CallArgs::deserialize(&bytes) {
                        let (address, value, input) = match call_args {
                            CallArgs::V2(args) => (args.contract, args.value, args.input),
                            CallArgs::V1(args) => (args.contract, WeiU256::default(), args.input),
                        };

                        tx = tx
                            .to(Some(address))
                            .nonce(U256::zero())
                            .gas_limit(U256::zero())
                            .max_priority_fee_per_gas(U256::zero())
                            .max_fee_per_gas(U256::zero())
                            .value(value.into())
                            .input(input)
                            .access_list(vec![])
                            .tx_type(0xff)
                            .contract_address(None)
                            .v(0)
                            .r(U256::zero())
                            .s(U256::zero());
                    } else {
                        tx = fill_tx(tx, "call", bytes);
                    }

                    fill_result(
                        tx,
                        &outcome.execution_outcome.outcome.status,
                        true,
                        &mut bloom,
                        txs.get(&hash),
                    )?
                }
                _ => {
                    hash = virtual_receipt_id.0.try_into().unwrap();
                    tx = tx.hash(hash).from(near_account_to_evm_address(
                        outcome.receipt.predecessor_id.as_bytes(),
                    ));
                    tx = fill_result(
                        tx,
                        &outcome.execution_outcome.outcome.status,
                        false,
                        &mut bloom,
                        txs.get(&hash),
                    )?;
                    fill_tx(tx, method_name, bytes)
                }
            }
        }
        action => {
            let input = action.try_to_vec().unwrap();

            tx = tx
                .hash(virtual_receipt_id.0.try_into().unwrap())
                .from(near_account_to_evm_address(
                    outcome.receipt.predecessor_id.as_bytes(),
                ))
                .to(Some(near_account_to_evm_address(b"aurora")))
                .contract_address(None)
                .nonce(U256::zero())
                .gas_limit(U256::max_value())
                .gas_used(0)
                .max_priority_fee_per_gas(U256::zero())
                .max_fee_per_gas(U256::zero())
                .value(Wei::new(U256::zero()))
                .input(input)
                .access_list(vec![])
                .tx_type(0xff)
                .logs(vec![])
                .v(0)
                .r(U256::zero())
                .s(U256::zero())
                // Type for NEAR custom transactions
                .tx_type(0xfe)
                .access_list(vec![]);

            match &outcome.execution_outcome.outcome.status {
                ExecutionStatusView::Unknown => {
                    tx = tx.output(vec![]).status(false);
                }
                ExecutionStatusView::Failure(err) => {
                    tx = tx.output(err.try_to_vec().unwrap()).status(false);
                }
                ExecutionStatusView::SuccessValue(value) => {
                    tx = tx.output(value.as_bytes().to_vec()).status(true);
                }
                ExecutionStatusView::SuccessReceiptId(data) => {
                    tx = tx.output(data.0.to_vec()).status(true);
                }
            }
        }
    }

    tx = tx.logs_bloom(bloom);

    Ok(BuiltTransaction {
        transaction: tx.build().map_err(RefinerError::BuilderError)?,
        transaction_hash,
    })
}

fn fill_with_submit_result(
    mut tx: AuroraTransactionBuilder,
    result: SubmitResult,
    blooms: &mut Bloom,
) -> AuroraTransactionBuilder {
    for log in result.logs.iter() {
        blooms.accrue_bloom(&get_log_blooms(log));
    }

    tx = tx.gas_used(result.gas_used).logs(result.logs);
    tx = match result.status {
        aurora_engine::parameters::TransactionStatus::Succeed(output) => {
            tx.status(true).output(output)
        }
        aurora_engine::parameters::TransactionStatus::Revert(output) => {
            tx.status(false).output(output)
        }
        _ => tx.status(false).output(vec![]),
    };
    tx
}

fn fill_result(
    mut tx: AuroraTransactionBuilder,
    status: &ExecutionStatusView,
    submit_or_call: bool,
    blooms: &mut Bloom,
    included_outcome: Option<&TransactionIncludedOutcome>,
) -> Result<AuroraTransactionBuilder, RefinerError> {
    if let Some(included_outcome) = included_outcome {
        if let Ok(execution_result) = included_outcome.maybe_result.as_ref() {
            if let Some(execution_result) = execution_result.as_ref() {
                return Ok(match execution_result {
                    TransactionExecutionResult::Submit(result) => match result {
                        Ok(result) => fill_with_submit_result(tx, result.clone(), blooms),
                        Err(err) => tx
                            .gas_used(err.gas_used)
                            .logs(vec![])
                            .status(false)
                            .output(format!("{:?}", err.kind).as_bytes().to_vec()),
                    },
                    TransactionExecutionResult::DeployErc20(address) => tx
                        .gas_used(0)
                        .logs(vec![])
                        .status(true)
                        .output(address.as_bytes().to_vec()),
                    TransactionExecutionResult::Promise(promise) => tx
                        .gas_used(0)
                        .logs(vec![])
                        .status(true)
                        .output(format!("{:?}", promise).as_bytes().to_vec()),
                });
            }
        }
    }

    match &status {
        ExecutionStatusView::SuccessValue(result) => {
            let result = base64::decode(&result).map_err(RefinerError::SuccessValueBase64Args)?;

            if submit_or_call {
                let result: SubmitResult = SubmitResult::try_from_slice(result.as_slice())
                    .map_err(RefinerError::SubmitResultArgs)?;
                tx = fill_with_submit_result(tx, result, blooms);
            } else {
                tx = tx.gas_used(0).logs(vec![]).status(true).output(result);
            }

            Ok(tx)
        }
        ExecutionStatusView::SuccessReceiptId(result) => Ok(tx
            .gas_used(0)
            .logs(vec![])
            .status(true)
            .output(result.0.to_vec())),
        _ => {
            crate::metrics::FAILING_NEAR_TRANSACTION.inc();
            tracing::warn!("Failing NEAR transaction: {:?}", status);
            Ok(tx.gas_used(0).logs(vec![]).status(false).output(
                vec![
                    b"ERR_NEAR_TX:".to_vec(),
                    format!("{:?}", status).as_bytes().to_vec(),
                ]
                .concat(),
            ))
        }
    }
}

fn fill_tx(
    tx: AuroraTransactionBuilder,
    method_name: &str,
    input: Vec<u8>,
) -> AuroraTransactionBuilder {
    tx.to(None)
        .nonce(U256::zero())
        .gas_limit(U256::zero())
        .max_priority_fee_per_gas(U256::zero())
        .max_fee_per_gas(U256::zero())
        .value(Wei::new(U256::zero()))
        .input(
            vec![
                method_name.to_string().as_bytes().to_vec(),
                b":".to_vec(),
                input,
            ]
            .concat(),
        )
        .access_list(vec![])
        .tx_type(0xff)
        .contract_address(None)
        .v(0)
        .r(U256::zero())
        .s(U256::zero())
}

#[derive(Debug)]
enum RefinerError {
    /// Fail building transaction. Most likely some arguments missing
    BuilderError(AuroraTransactionBuilderError),
    /// Failed to parse Ethereum Transaction
    ParseTransaction(ParseTransactionError),
    /// Failed to parse metadata from Ethereum Transaction
    ParseMetadata(rlp::DecoderError),
    /// Error decoding Function Call arguments
    FunctionCallBase64Args(base64::DecodeError),
    /// Error decoding Success Value from Receipt
    SuccessValueBase64Args(base64::DecodeError),
    /// Error decoding Submit Result
    SubmitResultArgs(std::io::Error),
}

fn get_log_blooms(log: &ResultLog) -> Bloom {
    let mut bloom = Bloom::default();
    bloom.accrue(log.address.as_bytes());
    for topic in log.topics.iter() {
        bloom.accrue(&topic[..]);
    }
    bloom
}

#[cfg(test)]
mod tests {
    use super::{compute_block_hash, compute_block_hash_preimage};

    #[test]
    fn test_block_hash_preimage() {
        assert_eq!(
            compute_block_hash_preimage(62482103, 1313161554),
            vec![
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 78, 69, 65, 82, 97, 117, 114, 111, 114, 97, 0, 0, 0, 0, 3, 185, 102, 183,
            ]
        );
    }

    #[test]
    fn test_block_hash() {
        // Example of block: https://explorer.mainnet.aurora.dev/block/62482103/transactions
        assert_eq!(
            hex::encode(compute_block_hash(62482103, 1313161554).as_bytes()),
            "97ccface51e97c896591c88ecb8106c4f48816493e1f7b1172245fb333a0e782"
        );
    }
}
