use crate::actors::dr_reporter::Report;
use crate::{
    actors::{
        dr_database::{DrDatabase, DrInfoBridge, DrState, GetAllPendingDrs, SetDrInfoBridge},
        dr_reporter::{DrReporter, DrReporterMsg},
    },
    config::Config,
};
use actix::prelude::*;
use serde_json::json;
use std::{convert::TryFrom, time::Duration};
use witnet_data_structures::chain::{Block, DataRequestInfo, Epoch, EpochConstants};
use witnet_net::client::tcp::{jsonrpc, JsonRpcClient};
use witnet_util::timestamp::get_timestamp;

/// WitPoller actor checks periodically the state of the requests in Witnet to call DrReporter
/// in case of found a tally
#[derive(Default)]
pub struct WitPoller {
    witnet_client: Option<Addr<JsonRpcClient>>,
    wit_tally_polling_rate_ms: u64,
    dr_tx_unresolved_timeout_ms: Option<u64>,
}

/// Make actor from WitPoller
impl Actor for WitPoller {
    /// Every actor has to provide execution Context in which it can run.
    type Context = Context<Self>;

    /// Method to be executed when the actor is started
    fn started(&mut self, ctx: &mut Self::Context) {
        log::debug!("WitPoller actor has been started!");

        self.check_tally_pending_drs(ctx, Duration::from_millis(self.wit_tally_polling_rate_ms))
    }
}

/// Required trait for being able to retrieve WitPoller address from system registry
impl actix::Supervised for WitPoller {}

/// Required trait for being able to retrieve WitPoller address from system registry
impl SystemService for WitPoller {}

impl WitPoller {
    /// Initialize the `WitPoller` taking the configuration from a `Config` structure
    /// and a Json-RPC client connected to a Witnet node
    pub fn from_config(config: &Config, node_client: Addr<JsonRpcClient>) -> Self {
        let wit_tally_polling_rate_ms = config.wit_tally_polling_rate_ms;
        let dr_tx_unresolved_timeout_ms = config.dr_tx_unresolved_timeout_ms;

        Self {
            witnet_client: Some(node_client),
            wit_tally_polling_rate_ms,
            dr_tx_unresolved_timeout_ms,
        }
    }

    fn check_tally_pending_drs(&self, ctx: &mut Context<Self>, period: Duration) {
        let witnet_client = self.witnet_client.clone().unwrap();
        let dr_tx_unresolved_timeout_ms = self.dr_tx_unresolved_timeout_ms;

        let fut = async move {
            let dr_database_addr = DrDatabase::from_registry();
            let dr_reporter_addr = DrReporter::from_registry();
            let pending_drs = dr_database_addr
                .send(GetAllPendingDrs)
                .await
                .unwrap()
                .unwrap();
            let current_timestamp = get_timestamp();
            let mut dr_reporter_msgs = vec![];

            for (dr_id, dr_bytes, dr_tx_hash, dr_tx_creation_timestamp) in pending_drs {
                let method = String::from("dataRequestReport");
                let params = json!([dr_tx_hash]);
                let req = jsonrpc::Request::method(method)
                    .timeout(Duration::from_millis(5_000))
                    .params(params)
                    .expect("params failed serialization");
                let report = witnet_client.send(req).await;
                let report = match report {
                    Ok(report) => report,
                    Err(_) => {
                        log::error!("Failed to connect to witnet client, will retry later");
                        break;
                    }
                };

                let report = match report {
                    Ok(report) => report,

                    Err(e) => {
                        log::debug!(
                            "[{}] dataRequestReport call error: {}",
                            dr_id,
                            e.to_string()
                        );

                        if let Some(dr_timeout_ms) = dr_tx_unresolved_timeout_ms {
                            // In case of error, if the data request has been unresolved for more than
                            // X milliseconds, retry by setting it to "New"
                            if (current_timestamp - dr_tx_creation_timestamp)
                                > i64::try_from(dr_timeout_ms / 1000).unwrap()
                            {
                                log::debug!("[{}] has been unresolved after more than {} ms, setting to New", dr_id, dr_timeout_ms);
                                dr_database_addr
                                    .send(SetDrInfoBridge(
                                        dr_id,
                                        DrInfoBridge {
                                            dr_bytes,
                                            dr_state: DrState::New,
                                            dr_tx_hash: None,
                                            dr_tx_creation_timestamp: None,
                                        },
                                    ))
                                    .await
                                    .unwrap();
                            }
                        }
                        continue;
                    }
                };

                match serde_json::from_value::<Option<DataRequestInfo>>(report) {
                    Ok(Some(DataRequestInfo {
                        tally: Some(tally),
                        block_hash_dr_tx: Some(dr_block_hash),
                        ..
                    })) => {
                        log::info!(
                            "[{}] Found possible tally to be reported for dr_tx_hash {}",
                            dr_id,
                            dr_tx_hash
                        );

                        let result = tally.tally;
                        // Get timestamp of first block with commits. The timestamp of the data
                        // point is the timestamp of that block minus 45 seconds, because the commit
                        // transactions are created one epoch earlier.
                        // TODO: first block with commits is hard to obtain, we are simply using the
                        // block that included the data request.
                        let timestamp = {
                            let method = String::from("getBlock");
                            let params = json!([dr_block_hash]);
                            let req = jsonrpc::Request::method(method)
                                .timeout(Duration::from_millis(5_000))
                                .params(params)
                                .expect("params failed serialization");
                            let report = witnet_client.send(req).await;
                            let report = match report {
                                Ok(report) => report,
                                Err(_) => {
                                    log::error!(
                                        "Failed to connect to witnet client, will retry later"
                                    );
                                    break;
                                }
                            };
                            let block = match report {
                                Ok(value) => serde_json::from_value::<Block>(value)
                                    .expect("failed to deserialize block"),
                                Err(e) => {
                                    log::error!(
                                        "error in getBlock call ({}): {:?}",
                                        dr_block_hash,
                                        e
                                    );
                                    continue;
                                }
                            };

                            let block_number = block.block_header.beacon.checkpoint;
                            // TODO: get constants from somewhere instead of hardcoding them here?
                            let epoch_constants = EpochConstants {
                                // Wednesday, 14-Oct-2020, 09:00 UTC
                                checkpoint_zero_timestamp: 1_602_666_000,
                                checkpoints_period: 45,
                            };

                            convert_block_epoch_to_timestamp(epoch_constants, block_number)
                        };

                        dr_reporter_msgs.push(Report {
                            dr_id,
                            timestamp,
                            dr_tx_hash,
                            result,
                        });
                    }
                    Ok(..) => {
                        // No problem, this means the data request has not been resolved yet
                        log::debug!("[{}] Data request not resolved yet", dr_id);
                        continue;
                    }
                    Err(e) => {
                        log::error!("[{}] dataRequestReport deserialize error: {:?}", dr_id, e);
                        continue;
                    }
                };
            }

            dr_reporter_addr
                .send(DrReporterMsg {
                    reports: dr_reporter_msgs,
                })
                .await
                .unwrap();
        };

        ctx.spawn(fut.into_actor(self).then(move |(), _act, ctx| {
            // Wait until the function finished to schedule next call.
            // This avoids tasks running in parallel.
            ctx.run_later(period, move |act, ctx| {
                // Reschedule check_tally_pending_drs
                act.check_tally_pending_drs(ctx, period);
            });

            actix::fut::ready(())
        }));
    }
}

fn convert_block_epoch_to_timestamp(epoch_constants: EpochConstants, epoch: Epoch) -> u64 {
    // In case of error, return timestamp 0
    u64::try_from(epoch_constants.epoch_timestamp(epoch).unwrap_or(0))
        .expect("Epoch timestamp should return a positive value")
}
