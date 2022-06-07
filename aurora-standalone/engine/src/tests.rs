use aurora_engine::parameters::TransactionStatus;
use aurora_engine_types::{account_id::AccountId, H256};
use aurora_refiner_types::near_block::NEARBlock;
use engine_standalone_storage::json_snapshot::{self, types::JsonSnapshot};
use engine_standalone_storage::sync::TransactionExecutionResult;
use engine_standalone_storage::Storage;
use std::collections::HashMap;

/// This test processes a real block from mainnet:
/// https://explorer.mainnet.near.org/blocks/GHxXqSq9RsV4UY6Cz4Hp64bBrqLtSAPXuzRck1KHZHH7
/// It includes a batched transaction, as well as some normal transactions from the relayer.
/// The purpose of this test is to check that all these transactions are handled correctly
/// (correctness based on the actual outcome on NEAR).
#[test]
fn test_batched_transactions() {
    let mut test_context =
        TestContext::load_snapshot("src/res/contract.aurora.block66381606.minimal.json");
    let block: NEARBlock = {
        let file = std::fs::File::open("src/res/block_66381607.json").unwrap();
        serde_json::from_reader(file).unwrap()
    };
    let mut data_id_mapping = lru::LruCache::new(1000);
    let mut outcomes_map = HashMap::new();
    crate::sync::consume_near_block(
        &mut test_context.storage,
        &block,
        &mut data_id_mapping,
        &test_context.engine_account_id,
        Some(&mut outcomes_map),
    )
    .unwrap();

    let expected_transactions: Vec<(H256, u64)> = [
        // Batched transactions:
        (
            "97b3a764e44319bc97c73df3e46d0af496953d9d1c26a083b9a911b310ce7d6f",
            21000_u64,
        ),
        (
            "ab9ebc74e4725116897281bec876248bde47d2bc72f5b7d6c39f4041aab55184",
            120998,
        ),
        (
            "ba3ece865b3411e9c6890c855d437cd05bd8c6c354cae9b048e272c5deda2f4d",
            239369,
        ),
        (
            "2028c462ec6c1ef387a81d3faad1d64a49e68f204cafb1986956a5e40b077aa7",
            96861,
        ),
        (
            "c474e6255a363b63d5ea2a3134de8c1fadbb52f0b1978b9a7cbb7fb73dcf1d38",
            21000,
        ),
        // single transactions:
        (
            "d57426403ab88ae3d4c22c0f3ed48d9aa60d92860afd6b6010f049a88042ae58",
            21000,
        ),
        (
            "fd66eaa68f8c2f13ff2b06fb4b11c3b300849b0a705ec783876d28b89594ff18",
            32512,
        ),
        (
            "4e6b28525424becc68f3fee764b60e2e5b971c7803a90e3a25780edf83523b82",
            294685,
        ),
        (
            "c55907c43879465f57bb8faebf6f6a48ecd8ec53c84222a762a6269d632aa32d",
            176023,
        ),
        (
            "1862a7ad8496881ef5e25d755c73c1f9bd62d62617160fcb009844bddba9334e",
            817921,
        ),
    ]
    .iter()
    .map(|(tx_hash, gas_used)| (H256::from_slice(&hex::decode(tx_hash).unwrap()), *gas_used))
    .collect();

    assert_eq!(expected_transactions.len(), outcomes_map.len());

    for (tx_hash, gas_used) in expected_transactions {
        let outcome = outcomes_map.remove(&tx_hash).unwrap();
        let submit_result = match outcome.maybe_result.unwrap().unwrap() {
            TransactionExecutionResult::Submit(x) => x.unwrap(),
            other => panic!("Unexpected result {:?}", other),
        };
        assert!(matches!(
            submit_result.status,
            TransactionStatus::Succeed(_)
        ));
        assert_eq!(submit_result.gas_used, gas_used);
    }

    test_context.close()
}

struct TestContext {
    storage: Storage,
    storage_path: tempfile::TempDir,
    engine_account_id: AccountId,
}

impl TestContext {
    fn load_snapshot(snapshot_path: &str) -> Self {
        let engine_account_id = "aurora".parse().unwrap();
        let storage_path = tempfile::tempdir().unwrap();
        let mut storage = Storage::open(storage_path.path()).unwrap();
        storage.set_engine_account_id(&engine_account_id).unwrap();
        let snapshot = JsonSnapshot::load_from_file(snapshot_path).unwrap();
        json_snapshot::initialize_engine_state(&mut storage, snapshot).unwrap();
        Self {
            storage,
            storage_path,
            engine_account_id,
        }
    }

    fn close(self) {
        drop(self.storage);
        self.storage_path.close().unwrap();
    }
}
