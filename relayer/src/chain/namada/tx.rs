use core::str::FromStr;
use core::time::Duration;
use std::path::Path;
use std::thread;
use std::time::Instant;

use ibc::events::IbcEvent;
use ibc::query::{QueryTxHash, QueryTxRequest};
use ibc_proto::google::protobuf::Any;
use namada::proto::Tx;
use namada::types::token::Amount;
use namada::types::transaction::{Fee, GasLimit, WrapperTx};
use namada_apps::wallet::Wallet;
use namada_apps::wasm_loader;
use tendermint::abci::transaction::Hash;
use tendermint_rpc::endpoint::broadcast::tx_sync::Response;
use tendermint_rpc_abciplus::endpoint::broadcast::tx_sync::Response as AbciPlusRpcResponse;
use tendermint_rpc_abciplus::Client;

use crate::chain::cosmos;
use crate::chain::cosmos::types::tx::TxSyncResult;
use crate::error::Error;

use super::super::ChainEndpoint;
use super::NamadaChain;
use super::BASE_WALLET_DIR;

const FEE_TOKEN: &str = "NAM";
const WASM_DIR: &str = "namada_wasm";
const WASM_FILE: &str = "tx_ibc.wasm";
const DEFAULT_MAX_GAS: u64 = 100_000;
const WAIT_BACKOFF: Duration = Duration::from_millis(300);

impl NamadaChain {
    pub fn send_tx(&mut self, proto_msg: &Any) -> Result<Response, Error> {
        let tx_code = wasm_loader::read_wasm(WASM_DIR, WASM_FILE).expect("Loading IBC wasm failed");
        let mut tx_data = vec![];
        prost::Message::encode(proto_msg, &mut tx_data)
            .map_err(|e| Error::protobuf_encode(String::from("Message"), e))?;
        let tx = Tx::new(tx_code, Some(tx_data));

        // the wallet should exist because it's confirmed when the bootstrap
        let wallet_path = Path::new(BASE_WALLET_DIR).join(self.config.id.to_string());
        let mut wallet = Wallet::load(&wallet_path).expect("wallet has not been initialized yet");
        // This also prevents a signer except for `config.key_name` from sending a token with relayer-cli
        let secret_key = wallet
            .find_key(&self.config.key_name)
            .map_err(Error::namada_key_pair_not_found)?;
        let signed_tx = tx.sign(&secret_key);

        let fee_token_addr = wallet
            .find_address(FEE_TOKEN)
            .ok_or_else(|| Error::namada_address_not_found(FEE_TOKEN.to_string()))?
            .clone();
        let relayer_addr = wallet
            .find_address(&self.config.key_name)
            .ok_or_else(|| Error::namada_address_not_found(self.config.key_name.clone()))?
            .clone();
        let balance = self.query_balance(&fee_token_addr, &relayer_addr)?;
        let fee_amount = self.query_tx_fee()?;
        if balance < fee_amount {
            return Err(Error::namada_tx_fee(balance, fee_amount));
        }

        // TODO estimate the gas cost?

        let gas_limit = GasLimit::from(self.config.max_gas.unwrap_or(DEFAULT_MAX_GAS));

        let epoch = self.query_epoch()?;
        let wrapper_tx = WrapperTx::new(
            Fee {
                amount: Amount::from(fee_amount),
                token: fee_token_addr,
            },
            &secret_key,
            epoch,
            gas_limit,
            signed_tx,
            Default::default(),
            None,
        );

        let tx = wrapper_tx
            .sign(&secret_key)
            .expect("Signing of the wrapper transaction should not fail");
        let tx_bytes = tx.to_bytes();

        let mut response = self
            .rt
            .block_on(self.rpc_client.broadcast_tx_sync(tx_bytes.into()))
            .map_err(|e| Error::abci_plus_rpc(self.config.rpc_addr.clone(), e))?;
        // overwrite the tx decrypted hash for the tx query
        response.hash = wrapper_tx.tx_hash.into();
        Ok(into_response(response))
    }

    pub fn wait_for_block_commits(
        &self,
        mut tx_sync_results: Vec<TxSyncResult>,
    ) -> Result<Vec<TxSyncResult>, Error> {
        let start_time = Instant::now();
        loop {
            if cosmos::wait::all_tx_results_found(&tx_sync_results) {
                return Ok(tx_sync_results);
            }

            let elapsed = start_time.elapsed();
            if elapsed > self.config.rpc_timeout {
                return Err(Error::tx_no_confirmation());
            }

            thread::sleep(WAIT_BACKOFF);

            for TxSyncResult { response, events } in tx_sync_results.iter_mut() {
                if cosmos::wait::empty_event_present(events) {
                    // If the transaction failed, replace the events with an error,
                    // so that we don't attempt to resolve the transaction later on.
                    if response.code.value() != 0 {
                        *events = vec![IbcEvent::ChainError(format!(
                            "deliver_tx on chain {} for Tx hash {} reports error: code={:?}, log={:?}",
                            self.id(), response.hash, response.code, response.log
                        ))];
                    // Otherwise, try to resolve transaction hash to the corresponding events.
                    } else if let Ok(events_per_tx) =
                        self.query_txs(QueryTxRequest::Transaction(QueryTxHash(response.hash)))
                    {
                        // If we get events back, progress was made, so we replace the events
                        // with the new ones. in both cases we will check in the next iteration
                        // whether or not the transaction was fully committed.
                        if !events_per_tx.is_empty() {
                            *events = events_per_tx;
                        }
                    }
                }
            }
        }
    }
}

/// Convert a broadcast response to one of the base Tendermint
fn into_response(resp: AbciPlusRpcResponse) -> Response {
    Response {
        code: u32::from(resp.code).into(),
        data: Vec::<u8>::from(resp.data).into(),
        log: tendermint::abci::Log::from(resp.log.as_ref()),
        hash: Hash::from_str(&resp.hash.to_string()).unwrap(),
    }
}
