use std::{panic, time::Instant};

use ethers::prelude::*;
use log::{error, info};

use crate::{
    db::{blob_db_manager::DBManager, mongodb::MongoDBManagerOptions},
    types::{Blob, BlockData, StdError, TransactionData},
    utils::{context::Context, web3::calculate_versioned_hash},
};

pub struct SlotProcessor<'a> {
    context: &'a Context,
    db_options: MongoDBManagerOptions,
}

impl<'a> SlotProcessor<'a> {
    pub async fn try_init(context: &'a Context) -> Result<SlotProcessor, StdError> {
        Ok(Self {
            context,
            db_options: MongoDBManagerOptions {
                session: context.db_manager.client.start_session(None).await?,
            },
        })
    }

    pub async fn process_slots(&mut self, start_slot: u32, end_slot: u32) {
        let mut current_slot = start_slot;

        while current_slot < end_slot {
            let result = self.process_slot(current_slot).await;

            // TODO: implement exponential backoff for proper error handling. If X intents have been made, then notify and stop process
            if let Err(e) = result {
                self.save_slot(current_slot - 1).await;

                error!("[Slot {}] Couldn't process slot: {}", current_slot, e);

                panic!();
            };

            current_slot = current_slot + 1;
        }

        self.save_slot(current_slot).await
    }

    async fn process_slot(&mut self, slot: u32) -> Result<(), StdError> {
        let Context {
            beacon_api,
            db_manager,
            provider,
        } = &mut self.context;

        let start = Instant::now();
        let beacon_block = match beacon_api.get_block(Some(slot)).await? {
            Some(block) => block,
            None => {
                info!("[Slot {}] Skipping as there is no beacon block", slot);

                return Ok(());
            }
        };

        let execution_payload = match beacon_block.body.execution_payload {
            Some(payload) => payload,
            None => {
                info!(
                    "[Slot {}] Skipping as beacon block doesn't contain execution payload",
                    slot
                );

                return Ok(());
            }
        };

        let blob_kzg_commitments = match beacon_block.body.blob_kzg_commitments {
            Some(commitments) => commitments,
            None => {
                info!(
                    "[Slot {}] Skipping as beacon block doesn't contain blob kzg commitments",
                    slot
                );

                return Ok(());
            }
        };
        let execution_block_hash = execution_payload.block_hash;

        let execution_block = match provider.get_block_with_txs(execution_block_hash).await? {
            Some(block) => block,
            None => {
                let error_msg = format!("Execution block {} not found", execution_block_hash);

                return Err(Box::new(ProviderError::CustomError(error_msg)));
            }
        };
        let block_data = BlockData::try_from((&execution_block, slot))?;

        if block_data.tx_to_versioned_hashes.is_empty() {
            info!(
                "[Slot {}] Skipping as execution block doesn't contain blob txs",
                slot
            );

            return Ok(());
        }

        let blobs = match beacon_api.get_blobs_sidecar(slot).await? {
            Some(blobs_sidecar) => {
                if blobs_sidecar.blobs.len() == 0 {
                    info!("[Slot {}] Skipping as blobs sidecar is empty", slot);

                    return Ok(());
                } else {
                    blobs_sidecar.blobs
                }
            }
            None => {
                info!("[Slot {}] Skipping as there is no blobs sidecar", slot);

                return Ok(());
            }
        };

        db_manager
            .start_transaction(Some(&mut self.db_options))
            .await?;

        db_manager
            .insert_block(&block_data, Some(&mut self.db_options))
            .await?;

        for tx in block_data.block.transactions.iter() {
            let blob_versioned_hashes = match block_data.tx_to_versioned_hashes.get(&tx.hash) {
                Some(versioned_hashes) => versioned_hashes,
                None => {
                    return Err(format!("Couldn't find versioned hashes for tx {}", tx.hash).into());
                }
            };

            db_manager
                .insert_tx(
                    &TransactionData {
                        tx,
                        blob_versioned_hashes,
                    },
                    Some(&mut self.db_options),
                )
                .await?;
        }

        for (i, blob) in blobs.iter().enumerate() {
            let commitment = blob_kzg_commitments[i].clone();

            let versioned_hash = calculate_versioned_hash(&commitment)?;

            match block_data.tx_to_versioned_hashes.iter().find_map(
                |(tx_hash, versioned_hashes)| match versioned_hashes.contains(&versioned_hash) {
                    true => Some(tx_hash),
                    false => None,
                },
            ) {
                Some(tx_hash) => {
                    db_manager
                        .insert_blob(
                            &Blob {
                                commitment,
                                data: blob,
                                versioned_hash,
                                tx_hash: tx_hash.clone(),
                            },
                            Some(&mut self.db_options),
                        )
                        .await?
                }
                None => {
                    let error_msg = format!(
                        "Couldn't find blob tx for commitment {} and versioned hash {}",
                        commitment, versioned_hash
                    );

                    return Err(Box::new(ProviderError::CustomError(error_msg)));
                }
            };
        }

        db_manager
            .commit_transaction(Some(&mut self.db_options))
            .await?;

        let duration = start.elapsed();

        info!(
            "[Slot {}] Blobs indexed correctly (elapsed time: {:?}s)",
            slot,
            duration.as_secs()
        );

        Ok(())
    }

    async fn save_slot(&mut self, slot: u32) {
        let result = self
            .context
            .db_manager
            .update_last_slot(slot, Some(&mut self.db_options))
            .await;

        if let Err(e) = result {
            error!("Couldn't update last slot: {}", e);
            panic!();
        }
    }
}
