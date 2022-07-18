use crate::metrics::{PROCESSED_BLOCKS, SKIP_BLOCKS};
use crate::refiner_inner::Refiner;
use aurora_refiner_types::aurora_block::AuroraBlock;
use aurora_refiner_types::near_block::NEARBlock;
use aurora_standalone_engine::EngineContext;

pub struct NearStream {
    /// Keep track of last block seen, to report empty blocks
    last_block_height: Option<u64>,
    /// Pass the filtered information to the handler
    handler: Refiner,
    /// Context used to access engine
    context: EngineContext,
}

impl NearStream {
    pub fn new(chain_id: u64, last_block_height: Option<u64>, context: EngineContext) -> Self {
        Self {
            last_block_height,
            handler: Refiner::new(chain_id),
            context,
        }
    }

    fn handle_block(&mut self, near_block: NEARBlock) -> AuroraBlock {
        self.handler.on_block_start(&near_block);

        let mut txs = Default::default();

        // Panic if engine can't consume this block
        aurora_standalone_engine::consume_near_block(
            &near_block,
            &mut self.context,
            Some(&mut txs),
        )
        .unwrap();

        near_block
            .shards
            .iter()
            .flat_map(|shard| shard.receipt_execution_outcomes.as_slice())
            .filter(|outcome| {
                outcome.receipt.receiver_id.as_bytes() == self.context.engine_account_id.as_bytes()
            })
            .for_each(|outcome| {
                self.handler
                    .on_execution_outcome(&near_block, outcome, &txs);
            });

        self.handler.on_block_end(&near_block)
    }

    pub fn next_block(&mut self, message: NEARBlock) -> Vec<AuroraBlock> {
        let mut blocks = vec![];

        let height = message.block.header.height;

        // Emit events for all skip blocks
        let mut last_height = self.last_block_height.unwrap_or(height);
        while last_height + 1 < height {
            last_height += 1;
            let skip_block = self.handler.on_block_skip(last_height, &message);
            blocks.push(skip_block);
            SKIP_BLOCKS.inc();
        }

        self.last_block_height = Some(height);
        let block = self.handle_block(message);
        blocks.push(block);
        PROCESSED_BLOCKS.inc();

        blocks
    }
}