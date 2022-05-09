use alloc::sync::Arc;
use core::str::FromStr;
use core::time::Duration;
use std::path::Path;
use std::thread;
use std::time::Instant;

use anoma::ledger::ibc::handler::commitment_prefix;
use anoma::ledger::ibc::storage;
use anoma::ledger::storage::{MerkleTree, Sha256Hasher};
use anoma::proto::Tx;
use anoma::types::address::{Address, InternalAddress};
use anoma::types::storage::{Epoch, Key, KeySeg, PrefixValue};
use anoma::types::token::Amount;
use anoma::types::transaction::GasLimit;
use anoma::types::transaction::{Fee, WrapperTx};
use anoma_apps::client::rpc::TxEventQuery;
use anoma_apps::node::ledger::rpc::Path as AnomaPath;
use anoma_apps::wallet::Wallet;
use anoma_apps::wasm_loader;
use borsh::BorshDeserialize;
use ibc::clients::ics07_tendermint::client_state::{AllowUpdate, ClientState};
use ibc::clients::ics07_tendermint::consensus_state::ConsensusState as TMConsensusState;
use ibc::clients::ics07_tendermint::header::Header as TmHeader;
use ibc::core::ics02_client::client_consensus::{AnyConsensusState, AnyConsensusStateWithHeight};
use ibc::core::ics02_client::client_state::{
    AnyClientState, ClientState as Ics02ClientState, IdentifiedAnyClientState,
};
use ibc::core::ics03_connection::connection::{ConnectionEnd, IdentifiedConnectionEnd};
use ibc::core::ics04_channel::channel::{ChannelEnd, IdentifiedChannelEnd};
use ibc::core::ics04_channel::packet::{PacketMsgType, Sequence};
use ibc::core::ics23_commitment::commitment::CommitmentPrefix;
use ibc::core::ics23_commitment::merkle::convert_tm_to_ics_merkle_proof;
use ibc::core::ics24_host::identifier::{
    ChainId, ChannelId, ClientId, ConnectionId, PortChannelId, PortId,
};
use ibc::events::{from_tx_response_event, IbcEvent};
use ibc::query::QueryBlockRequest;
use ibc::query::{QueryTxHash, QueryTxRequest};
use ibc::signer::Signer;
use ibc::Height as ICSHeight;
use ibc_proto::google::protobuf::Any;
use ibc_proto::ibc::core::channel::v1::{
    PacketState, QueryChannelClientStateRequest, QueryChannelsRequest,
    QueryConnectionChannelsRequest, QueryNextSequenceReceiveRequest,
    QueryPacketAcknowledgementsRequest, QueryPacketCommitmentsRequest, QueryUnreceivedAcksRequest,
    QueryUnreceivedPacketsRequest,
};
use ibc_proto::ibc::core::client::v1::{QueryClientStatesRequest, QueryConsensusStatesRequest};
use ibc_proto::ibc::core::commitment::v1::MerkleProof;
use ibc_proto::ibc::core::connection::v1::{
    QueryClientConnectionsRequest, QueryConnectionsRequest,
};
use tendermint::abci::tag::Tag;
use tendermint::abci::transaction::Hash;
use tendermint::abci::Code;
use tendermint::block::Height;
use tendermint_light_client::types::LightBlock as TMLightBlock;
use tendermint_proto::Protobuf;
use tendermint_rpc::query::{EventType, Query};
use tendermint_rpc::{endpoint::broadcast::tx_sync::Response, Client, HttpClient, Order};
use tokio::runtime::Runtime as TokioRuntime;

use super::tx::TrackedMsgs;
use crate::chain::client::ClientSettings;
use crate::chain::cosmos;
use crate::chain::cosmos::types::tx::TxSyncResult;
use crate::chain::ChainStatus;
use crate::config::ChainConfig;
use crate::error::Error;
use crate::event::monitor::TxMonitorCmd;
use crate::event::monitor::{EventMonitor, EventReceiver};
use crate::keyring::{KeyEntry, KeyRing};
use crate::light_client::tendermint::LightClient as TmLightClient;
use crate::light_client::LightClient;
use crate::light_client::Verified;

use super::{ChainEndpoint, HealthCheck};

const BASE_WALLET_DIR: &str = "anoma_wallet";
const WASM_DIR: &str = "anoma_wasm";
const WASM_FILE: &str = "tx_ibc.wasm";
const FEE_TOKEN: &str = "XAN";
const DEFAULT_MAX_GAS: u64 = 100_000;
const WAIT_BACKOFF: Duration = Duration::from_millis(300);

pub struct AnomaChain {
    config: ChainConfig,
    rpc_client: HttpClient,
    rt: Arc<TokioRuntime>,
    keybase: KeyRing,
}

impl AnomaChain {
    fn send_tx(&mut self, proto_msg: &Any) -> Result<Response, Error> {
        let tx_code = wasm_loader::read_wasm(WASM_DIR, WASM_FILE);
        let mut tx_data = vec![];
        prost::Message::encode(proto_msg, &mut tx_data)
            .map_err(|e| Error::protobuf_encode(String::from("Message"), e))?;
        let tx = Tx::new(tx_code, Some(tx_data));

        // TODO set the wallet to self
        // TODO when no account is initialized
        let wallet_path = Path::new(BASE_WALLET_DIR).join(self.config.id.to_string());
        let mut wallet = Wallet::load_or_new(&wallet_path);

        // TODO key for this relayer
        let secret_key = wallet.find_key("albert-key").map_err(Error::anoma_wallet)?;
        let signed_tx = tx.sign(&secret_key);
        let tx_hash = signed_tx.hash();

        let fee_token_addr = wallet
            .find_address(FEE_TOKEN)
            .ok_or_else(|| Error::anoma_address(FEE_TOKEN.to_string()))?
            .clone();

        // TODO estimate the gas cost?

        let gas_limit = GasLimit::from(self.config.max_gas.unwrap_or(DEFAULT_MAX_GAS));

        let epoch = self.query_epoch()?;
        let wrapper_tx = WrapperTx::new(
            Fee {
                amount: Amount::from(0),
                token: fee_token_addr,
            },
            &secret_key,
            epoch,
            gas_limit,
            signed_tx,
            Default::default(),
        );

        let tx = wrapper_tx
            .sign(&secret_key)
            .expect("Signing of the wrapper transaction should not fail");
        let tx_bytes = tx.to_bytes();

        let mut response = self
            .rt
            .block_on(self.rpc_client.broadcast_tx_sync(tx_bytes.into()))
            .map_err(|e| Error::rpc(self.config.rpc_addr.clone(), e))?;
        // overwrite the tx hash in the response
        response.hash = Hash::new(tx_hash);
        Ok(response)
    }

    fn wait_for_block_commits(
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

    fn query(
        &self,
        key: Key,
        height: Option<ICSHeight>,
        prove: bool,
    ) -> Result<(Vec<u8>, Option<MerkleProof>), Error> {
        let path = AnomaPath::Value(key);
        let height = match height {
            Some(h) => Some(Height::try_from(h.revision_height).map_err(Error::invalid_height)?),
            None => None,
        };
        let data = vec![];
        let response = self
            .rt
            .block_on(
                self.rpc_client
                    .abci_query(Some(path.into()), data, height, prove),
            )
            .map_err(|e| Error::rpc(self.config.rpc_addr.clone(), e))?;
        let value = match response.code {
            Code::Ok => response.value,
            Code::Err(1) => vec![],
            Code::Err(_) => return Err(Error::abci_query(response)),
        };

        let proof = if prove {
            let p = response.proof.ok_or_else(Error::empty_response_proof)?;
            Some(convert_tm_to_ics_merkle_proof(&p).map_err(Error::ics23)?)
        } else {
            None
        };

        Ok((value, proof))
    }

    fn query_prefix(&self, prefix: Key) -> Result<Vec<PrefixValue>, Error> {
        let path = AnomaPath::Prefix(prefix);
        let data = vec![];
        let response = self
            .rt
            .block_on(
                self.rpc_client
                    .abci_query(Some(path.into()), data, None, false),
            )
            .map_err(|e| Error::rpc(self.config.rpc_addr.clone(), e))?;
        match response.code {
            Code::Ok => {
                Vec::<PrefixValue>::try_from_slice(&response.value[..]).map_err(Error::borsh_decode)
            }
            Code::Err(c) if c == 1 => Ok(vec![]),
            _ => Err(Error::abci_query(response)),
        }
    }

    fn query_epoch(&self) -> Result<Epoch, Error> {
        let path = AnomaPath::Epoch;
        let data = vec![];
        let response = self
            .rt
            .block_on(
                self.rpc_client
                    .abci_query(Some(path.into()), data, None, false),
            )
            .map_err(|e| Error::rpc(self.config.rpc_addr.clone(), e))?;
        match response.code {
            Code::Ok => Epoch::try_from_slice(&response.value[..]).map_err(Error::borsh_decode),
            Code::Err(_) => Err(Error::abci_query(response)),
        }
    }

    fn query_events(&self, query: Query) -> Result<Vec<IbcEvent>, Error> {
        let blocks = &self
            .rt
            .block_on(self.rpc_client.block_search(query, 1, 1, Order::Ascending))
            .map_err(|e| Error::rpc(self.config.rpc_addr.clone(), e))?
            .blocks;
        let block = &blocks
            .get(0)
            .ok_or_else(|| Error::query("No block was found".to_string()))?
            .block;
        let response = self
            .rt
            .block_on(self.rpc_client.block_results(block.header.height))
            .map_err(|e| Error::rpc(self.config.rpc_addr.clone(), e))?;

        let events = response
            .end_block_events
            .ok_or_else(|| Error::query("No transaction result was found".to_string()))?;
        let mut ibc_events = vec![];
        for event in &events {
            let height = ICSHeight::new(self.id().version(), u64::from(response.height));
            match from_tx_response_event(height, event) {
                Some(e) => ibc_events.push(e),
                None => {
                    let success_code_tag = Tag {
                        key: "code".parse().expect("The tag parsing shouldn't fail"),
                        value: "0".parse().expect("The tag parsing shouldn't fail"),
                    };
                    if !event.attributes.contains(&success_code_tag) {
                        ibc_events.push(IbcEvent::ChainError(format!(
                            "The transaction was invalid: event {:?}",
                            event
                        )));
                    }
                }
            }
        }
        Ok(ibc_events)
    }
}

impl ChainEndpoint for AnomaChain {
    type LightBlock = TMLightBlock;
    type Header = TmHeader;
    type ConsensusState = TMConsensusState;
    type ClientState = ClientState;
    type LightClient = TmLightClient;

    fn bootstrap(config: ChainConfig, rt: Arc<TokioRuntime>) -> Result<Self, Error> {
        let rpc_client = HttpClient::new(config.rpc_addr.clone())
            .map_err(|e| Error::rpc(config.rpc_addr.clone(), e))?;

        // not used in Anoma, but the trait requires KeyRing
        let keybase = KeyRing::new(config.key_store_type, &config.account_prefix, &config.id)
            .map_err(Error::key_base)?;

        // overwrite the proof spec
        // TODO: query the proof spec
        let config = ChainConfig {
            proof_specs: MerkleTree::<Sha256Hasher>::default().proof_specs().into(),
            ..config
        };

        Ok(Self {
            config,
            rpc_client,
            rt,
            keybase,
        })
    }

    fn init_light_client(&self) -> Result<Self::LightClient, Error> {
        use tendermint_light_client::types::PeerId;
        let peer_id: PeerId = self
            .rt
            .block_on(self.rpc_client.status())
            .map(|s| s.node_info.id)
            .map_err(|e| Error::rpc(self.config.rpc_addr.clone(), e))?;

        let light_client = TmLightClient::from_config(&self.config, peer_id)?;

        Ok(light_client)
    }

    fn init_event_monitor(
        &self,
        rt: Arc<TokioRuntime>,
    ) -> Result<(EventReceiver, TxMonitorCmd), Error> {
        // TODO same as cosmos.rs
        let (mut event_monitor, event_receiver, monitor_tx) = EventMonitor::new(
            self.config.id.clone(),
            self.config.websocket_addr.clone(),
            rt,
        )
        .map_err(Error::event_monitor)?;

        event_monitor.subscribe().map_err(Error::event_monitor)?;

        thread::spawn(move || event_monitor.run());

        Ok((event_receiver, monitor_tx))
    }

    fn id(&self) -> &ChainId {
        &self.config.id
    }

    fn shutdown(self) -> Result<(), Error> {
        Ok(())
    }

    fn health_check(&self) -> Result<HealthCheck, Error> {
        self.rt.block_on(self.rpc_client.health()).map_err(|e| {
            Error::health_check_json_rpc(
                self.config.id.clone(),
                self.config.rpc_addr.to_string(),
                "/health".to_string(),
                e,
            )
        })?;

        self.rt
            .block_on(self.rpc_client.tx_search(
                Query::from(EventType::NewBlock),
                false,
                1,
                1,
                Order::Ascending,
            ))
            .map_err(|e| {
                Error::health_check_json_rpc(
                    self.config.id.clone(),
                    self.config.rpc_addr.to_string(),
                    "/tx_search".to_string(),
                    e,
                )
            })?;

        // TODO version check

        Ok(HealthCheck::Healthy)
    }

    fn keybase(&self) -> &KeyRing {
        &self.keybase
    }

    fn keybase_mut(&mut self) -> &mut KeyRing {
        &mut self.keybase
    }

    fn send_messages_and_wait_commit(
        &mut self,
        tracked_msgs: TrackedMsgs,
    ) -> Result<Vec<IbcEvent>, Error> {
        let proto_msgs = tracked_msgs.messages();
        if proto_msgs.is_empty() {
            return Ok(vec![]);
        }
        let mut tx_sync_results = vec![];
        for msg in proto_msgs.iter() {
            let events_per_tx = vec![IbcEvent::default(); proto_msgs.len()];
            let response = self.send_tx(msg)?;
            tx_sync_results.push(TxSyncResult {
                response,
                events: events_per_tx,
            });
        }

        let tx_sync_results = self.wait_for_block_commits(tx_sync_results)?;

        let events: Vec<IbcEvent> = tx_sync_results
            .into_iter()
            .map(|el| el.events)
            .flatten()
            .collect();
        let mut dedup_events = vec![];
        for event in events {
            if !dedup_events.contains(&event) {
                dedup_events.push(event);
            }
        }

        Ok(dedup_events)
    }

    fn send_messages_and_wait_check_tx(
        &mut self,
        tracked_msgs: TrackedMsgs,
    ) -> Result<Vec<Response>, Error> {
        let proto_msgs = tracked_msgs.messages();
        if proto_msgs.is_empty() {
            return Ok(vec![]);
        }
        let mut responses = vec![];
        for msg in proto_msgs.iter() {
            responses.push(self.send_tx(msg)?);
        }

        Ok(responses)
    }

    fn get_signer(&mut self) -> Result<Signer, Error> {
        // TODO set the wallet to self
        let wallet_path = Path::new(BASE_WALLET_DIR).join(self.config.id.to_string());
        let wallet = Wallet::load_or_new(&wallet_path);
        let address = wallet
            .find_address(&self.config.key_name)
            .ok_or_else(|| Error::anoma_address(self.config.key_name.clone()))?;

        Ok(Signer::new(address))
    }

    fn config(&self) -> ChainConfig {
        self.config.clone()
    }

    fn get_key(&mut self) -> Result<KeyEntry, Error> {
        // TODO get the key from the wallet
        let key = self
            .keybase()
            .get_key(&self.config.key_name)
            .map_err(|e| Error::key_not_found(self.config.key_name.clone(), e))?;

        Ok(key)
    }

    fn add_key(&mut self, key_name: &str, key: KeyEntry) -> Result<(), Error> {
        // TODO add the key to the wallet
        self.keybase_mut()
            .add_key(key_name, key)
            .map_err(Error::key_base)?;

        Ok(())
    }

    fn query_commitment_prefix(&self) -> Result<CommitmentPrefix, Error> {
        Ok(commitment_prefix())
    }

    fn query_status(&self) -> Result<ChainStatus, Error> {
        let status = self
            .rt
            .block_on(self.rpc_client.status())
            .map_err(|e| Error::rpc(self.config.rpc_addr.clone(), e))?;

        if status.sync_info.catching_up {
            return Err(Error::chain_not_caught_up(
                self.config.rpc_addr.to_string(),
                self.config().id,
            ));
        }

        let time = status.sync_info.latest_block_time;
        let height = ICSHeight {
            revision_number: ChainId::chain_version(status.node_info.network.as_str()),
            revision_height: u64::from(status.sync_info.latest_block_height),
        };

        Ok(ChainStatus {
            height,
            timestamp: time.into(),
        })
    }

    fn query_clients(
        &self,
        _request: QueryClientStatesRequest,
    ) -> Result<Vec<IdentifiedAnyClientState>, Error> {
        let prefix = ibc_key("clients")?;
        let mut states = vec![];
        for prefix_value in self.query_prefix(prefix)? {
            let PrefixValue { key, value } = prefix_value;
            if key.to_string().ends_with("clientState") {
                let client_id =
                    storage::client_id(&key).map_err(|e| Error::query(e.to_string()))?;
                let client_state = AnyClientState::decode_vec(&value).map_err(Error::decode)?;
                states.push(IdentifiedAnyClientState::new(client_id, client_state));
            }
        }

        Ok(states)
    }

    fn query_client_state(
        &self,
        client_id: &ClientId,
        height: ICSHeight,
    ) -> Result<AnyClientState, Error> {
        let key = storage::client_state_key(client_id);
        let (value, _) = self.query(key, Some(height), false)?;
        AnyClientState::decode_vec(&value).map_err(Error::decode)
    }

    fn query_consensus_states(
        &self,
        _request: QueryConsensusStatesRequest,
    ) -> Result<Vec<AnyConsensusStateWithHeight>, Error> {
        let prefix = ibc_key("clients")?;
        let mut states = vec![];
        for prefix_value in self.query_prefix(prefix)? {
            let PrefixValue { key, value } = prefix_value;
            let height = match storage::consensus_height(&key) {
                Ok(h) => h,
                // the key is not for a consensus state
                Err(_) => continue,
            };
            let consensus_state = AnyConsensusState::decode_vec(&value).map_err(Error::decode)?;
            states.push(AnyConsensusStateWithHeight {
                height,
                consensus_state,
            });
        }

        Ok(states)
    }

    fn query_consensus_state(
        &self,
        client_id: ClientId,
        consensus_height: ICSHeight,
        query_height: ICSHeight,
    ) -> Result<AnyConsensusState, Error> {
        let key = storage::consensus_state_key(&client_id, consensus_height);
        let (value, _) = self.query(key, Some(query_height), false)?;
        AnyConsensusState::decode_vec(&value).map_err(Error::decode)
    }

    fn query_upgraded_client_state(
        &self,
        _height: ICSHeight,
    ) -> Result<(AnyClientState, MerkleProof), Error> {
        unimplemented!()
    }

    fn query_upgraded_consensus_state(
        &self,
        _height: ICSHeight,
    ) -> Result<(AnyConsensusState, MerkleProof), Error> {
        unimplemented!()
    }

    fn query_connections(
        &self,
        _request: QueryConnectionsRequest,
    ) -> Result<Vec<IdentifiedConnectionEnd>, Error> {
        let prefix = ibc_key("connections")?;
        let mut connections = vec![];
        for prefix_value in self.query_prefix(prefix)? {
            let PrefixValue { key, value } = prefix_value;
            // "connections/counter" should be skipped
            if key == storage::connection_counter_key() {
                continue;
            }
            let connection_id =
                storage::connection_id(&key).map_err(|e| Error::query(e.to_string()))?;
            let connection = ConnectionEnd::decode_vec(&value).map_err(Error::decode)?;
            connections.push(IdentifiedConnectionEnd::new(connection_id, connection));
        }

        Ok(connections)
    }

    fn query_client_connections(
        &self,
        request: QueryClientConnectionsRequest,
    ) -> Result<Vec<ConnectionId>, Error> {
        let client_id =
            ClientId::from_str(&request.client_id).map_err(|e| Error::query(e.to_string()))?;
        // TODO needs to store connection IDs for each client in Anoma
        // workaround by getting all connections
        let request = QueryConnectionsRequest { pagination: None };
        let connections = self.query_connections(request)?;
        let ids = connections
            .iter()
            .filter(|c| *c.connection_end.client_id() == client_id)
            .map(|c| c.connection_id.clone())
            .collect();
        Ok(ids)
    }

    fn query_connection(
        &self,
        connection_id: &ConnectionId,
        height: ICSHeight,
    ) -> Result<ConnectionEnd, Error> {
        let key = storage::connection_key(connection_id);
        let (value, _) = self.query(key, Some(height), false)?;
        ConnectionEnd::decode_vec(&value).map_err(Error::decode)
    }

    fn query_connection_channels(
        &self,
        request: QueryConnectionChannelsRequest,
    ) -> Result<Vec<IdentifiedChannelEnd>, Error> {
        let connection_id =
            vec![ConnectionId::from_str(&request.connection)
                .map_err(|e| Error::query(e.to_string()))?];
        let req = QueryChannelsRequest { pagination: None };
        let channels = self
            .query_channels(req)?
            .into_iter()
            .filter(|c| c.channel_end.connection_hops_matches(&connection_id))
            .collect();

        Ok(channels)
    }

    fn query_channels(
        &self,
        _request: QueryChannelsRequest,
    ) -> Result<Vec<IdentifiedChannelEnd>, Error> {
        let prefix = ibc_key("channelEnds")?;
        let mut channels = vec![];
        for prefix_value in self.query_prefix(prefix)? {
            let PrefixValue { key, value } = prefix_value;
            // "channelEnds/counter" should be skipped
            if key == storage::channel_counter_key() {
                continue;
            }
            let port_channel_id =
                storage::port_channel_id(&key).map_err(|e| Error::query(e.to_string()))?;
            let channel = ChannelEnd::decode_vec(&value).map_err(Error::decode)?;
            channels.push(IdentifiedChannelEnd::new(
                port_channel_id.port_id.clone(),
                port_channel_id.channel_id,
                channel,
            ))
        }

        Ok(channels)
    }

    fn query_channel(
        &self,
        port_id: &PortId,
        channel_id: &ChannelId,
        height: ICSHeight,
    ) -> Result<ChannelEnd, Error> {
        let port_channel_id = PortChannelId {
            port_id: port_id.clone(),
            channel_id: channel_id.clone(),
        };
        let key = storage::channel_key(&port_channel_id);
        let (value, _) = self.query(key, Some(height), false)?;
        ChannelEnd::decode_vec(&value).map_err(Error::decode)
    }

    fn query_channel_client_state(
        &self,
        request: QueryChannelClientStateRequest,
    ) -> Result<Option<IdentifiedAnyClientState>, Error> {
        let port_id = PortId::from_str(&request.port_id)
            .map_err(|_| Error::query(format!("invalid port ID {}", request.port_id)))?;
        let channel_id = ChannelId::from_str(&request.channel_id)
            .map_err(|_| Error::query(format!("invalid channel ID {}", request.channel_id)))?;
        let channel_end = self.query_channel(&port_id, &channel_id, ICSHeight::default())?;
        let connection_id = channel_end
            .connection_hops()
            .get(0)
            .ok_or_else(|| Error::query("no connection ID in the channel end".to_string()))?;
        let connection_end = self.query_connection(connection_id, ICSHeight::default())?;
        let client_id = connection_end.client_id();
        let client_state = self.query_client_state(client_id, ICSHeight::default())?;

        Ok(Some(IdentifiedAnyClientState {
            client_id: client_id.clone(),
            client_state: client_state.wrap_any(),
        }))
    }

    fn query_packet_commitments(
        &self,
        request: QueryPacketCommitmentsRequest,
    ) -> Result<(Vec<PacketState>, ICSHeight), Error> {
        let path = format!(
            "commitments/ports/{}/channels/{}/sequences",
            request.port_id, request.channel_id
        );
        let prefix = ibc_key(path)?;
        let mut states = vec![];
        for prefix_value in self.query_prefix(prefix)? {
            let PrefixValue { key, value } = prefix_value;
            let (port_id, channel_id, sequence) =
                storage::port_channel_sequence_id(&key).map_err(|e| Error::query(e.to_string()))?;
            states.push(PacketState {
                port_id: port_id.to_string(),
                channel_id: channel_id.to_string(),
                sequence: sequence.into(),
                data: value,
            });
        }

        // TODO the height might be mismatched with the previous query
        let status = self.query_status()?;

        Ok((states, status.height))
    }

    fn query_unreceived_packets(
        &self,
        request: QueryUnreceivedPacketsRequest,
    ) -> Result<Vec<u64>, Error> {
        let path = format!(
            "receipts/ports/{}/channels/{}/sequences",
            request.port_id, request.channel_id
        );
        let prefix = ibc_key(path)?;
        let mut received_seqs = vec![];
        for prefix_value in self.query_prefix(prefix)? {
            let (_, _, sequence) = storage::port_channel_sequence_id(&prefix_value.key)
                .map_err(|e| Error::query(e.to_string()))?;
            received_seqs.push(u64::from(sequence))
        }

        let unreceived_seqs = request
            .packet_commitment_sequences
            .into_iter()
            .filter(|seq| !received_seqs.contains(seq))
            .collect();

        Ok(unreceived_seqs)
    }

    fn query_packet_acknowledgements(
        &self,
        request: QueryPacketAcknowledgementsRequest,
    ) -> Result<(Vec<PacketState>, ICSHeight), Error> {
        let path = format!(
            "acks/ports/{}/channels/{}/sequences",
            request.port_id, request.channel_id
        );
        let prefix = ibc_key(path)?;
        let mut states = vec![];
        for prefix_value in self.query_prefix(prefix)? {
            let PrefixValue { key, value } = prefix_value;
            let (port_id, channel_id, sequence) =
                storage::port_channel_sequence_id(&key).map_err(|e| Error::query(e.to_string()))?;
            let seq = u64::from(sequence);
            if request.packet_commitment_sequences.contains(&seq) {
                states.push(PacketState {
                    port_id: port_id.to_string(),
                    channel_id: channel_id.to_string(),
                    sequence: sequence.into(),
                    data: value,
                });
            }
        }

        // TODO the height might be mismatched with the previous query
        let status = self.query_status()?;

        Ok((states, status.height))
    }

    fn query_unreceived_acknowledgements(
        &self,
        request: QueryUnreceivedAcksRequest,
    ) -> Result<Vec<u64>, Error> {
        let path = format!(
            "commitments/ports/{}/channels/{}/sequences",
            request.port_id, request.channel_id
        );
        let prefix = ibc_key(path)?;
        let mut unreceived_seqs = vec![];
        for prefix_value in self.query_prefix(prefix)? {
            let PrefixValue { key, value: _ } = prefix_value;
            let (_, _, sequence) =
                storage::port_channel_sequence_id(&key).map_err(|e| Error::query(e.to_string()))?;
            let seq = u64::from(sequence);
            if request.packet_ack_sequences.contains(&seq) {
                unreceived_seqs.push(seq);
            }
        }

        Ok(unreceived_seqs)
    }

    fn query_next_sequence_receive(
        &self,
        request: QueryNextSequenceReceiveRequest,
    ) -> Result<Sequence, Error> {
        let port_id = PortId::from_str(&request.port_id)
            .map_err(|_| Error::query(format!("invalid port ID {}", request.port_id)))?;
        let channel_id = ChannelId::from_str(&request.channel_id)
            .map_err(|_| Error::query(format!("invalid channel ID {}", request.channel_id)))?;
        let port_channel_id = PortChannelId {
            port_id,
            channel_id,
        };
        let key = storage::next_sequence_recv_key(&port_channel_id);
        let (value, _) = self.query(key, None, false)?;

        // As ibc-go, the sequence index is encoded with big-endian
        let index: [u8; 8] = value
            .try_into()
            .map_err(|_| Error::query("Encoding u64 failed".to_owned()))?;
        let seq = u64::from_be_bytes(index).into();

        Ok(seq)
    }

    fn query_txs(&self, request: QueryTxRequest) -> Result<Vec<IbcEvent>, Error> {
        match request {
            QueryTxRequest::Packet(request) => {
                let mut result: Vec<IbcEvent> = vec![];
                for seq in &request.sequences {
                    // query first (and only) Tx that includes the event specified in the query request
                    let events = self.query_events(cosmos::query::packet_query(&request, *seq))?;
                    let events: Vec<IbcEvent> = events
                        .into_iter()
                        .filter(|e| e.event_type().as_str() == request.event_id.as_str())
                        .collect();
                    for event in events {
                        if !result.contains(&event) {
                            result.push(event);
                        }
                    }
                }
                Ok(result)
            }

            QueryTxRequest::Client(request) => {
                crate::time!("query_txs: single client update event");
                let mut response = self
                    .rt
                    .block_on(self.rpc_client.tx_search(
                        cosmos::query::header_query(&request),
                        false,
                        1,
                        1, // get only the first Tx matching the query
                        Order::Ascending,
                    ))
                    .map_err(|e| Error::rpc(self.config.rpc_addr.clone(), e))?;

                if response.txs.is_empty() {
                    return Ok(vec![]);
                }

                // the response must include a single Tx as specified in the query.
                assert!(
                    response.txs.len() <= 1,
                    "packet_from_tx_search_response: unexpected number of txs"
                );

                let tx = response.txs.remove(0);
                let event = cosmos::query::tx::update_client_from_tx_search_response(
                    self.id(),
                    &request,
                    tx,
                );

                Ok(event.into_iter().collect())
            }

            QueryTxRequest::Transaction(tx) => {
                let tx_query = TxEventQuery::Applied(tx.0.to_string());
                let events = self.query_events(Query::from(tx_query.clone()))?;
                Ok(events)
            }
        }
    }

    fn query_blocks(
        &self,
        request: QueryBlockRequest,
    ) -> Result<(Vec<IbcEvent>, Vec<IbcEvent>), Error> {
        // TODO same as cosmos.rs
        match request {
            QueryBlockRequest::Packet(request) => {
                crate::time!("query_blocks: query block packet events");

                let mut begin_block_events: Vec<IbcEvent> = vec![];
                let mut end_block_events: Vec<IbcEvent> = vec![];

                for seq in &request.sequences {
                    let response = self
                        .rt
                        .block_on(self.rpc_client.block_search(
                            cosmos::query::packet_query(&request, *seq),
                            1,
                            1, // there should only be a single match for this query
                            Order::Ascending,
                        ))
                        .map_err(|e| Error::rpc(self.config.rpc_addr.clone(), e))?;

                    assert!(
                        response.blocks.len() <= 1,
                        "block_results: unexpected number of blocks"
                    );

                    if let Some(block) = response.blocks.first().map(|first| &first.block) {
                        let response_height =
                            ICSHeight::new(self.id().version(), u64::from(block.header.height));

                        if request.height != ICSHeight::zero() && response_height > request.height {
                            continue;
                        }

                        let response = self
                            .rt
                            .block_on(self.rpc_client.block_results(block.header.height))
                            .map_err(|e| Error::rpc(self.config.rpc_addr.clone(), e))?;

                        begin_block_events.append(
                            &mut response
                                .begin_block_events
                                .unwrap_or_default()
                                .into_iter()
                                .filter_map(|ev| cosmos::filter_matching_event(ev, &request, *seq))
                                .collect(),
                        );

                        end_block_events.append(
                            &mut response
                                .end_block_events
                                .unwrap_or_default()
                                .into_iter()
                                .filter_map(|ev| cosmos::filter_matching_event(ev, &request, *seq))
                                .collect(),
                        );
                    }
                }
                Ok((begin_block_events, end_block_events))
            }
        }
    }

    fn query_host_consensus_state(&self, height: ICSHeight) -> Result<Self::ConsensusState, Error> {
        // TODO same as cosmos.rs
        let height = Height::try_from(height.revision_height).map_err(Error::invalid_height)?;

        // TODO(hu55a1n1): use the `/header` RPC endpoint instead when we move to tendermint v0.35.x
        let rpc_call = match height.value() {
            0 => self.rpc_client.latest_block(),
            _ => self.rpc_client.block(height),
        };
        let response = self.rt
            .block_on(rpc_call)
            .map_err(|e| Error::rpc(self.config.rpc_addr.clone(), e))?;
        Ok(response.block.header.into())
    }

    fn proven_client_state(
        &self,
        client_id: &ClientId,
        height: ICSHeight,
    ) -> Result<(AnyClientState, MerkleProof), Error> {
        let key = storage::client_state_key(client_id);
        let (value, proof) = self.query(key, Some(height), true)?;
        let client_state = AnyClientState::decode_vec(&value).map_err(Error::decode)?;

        Ok((client_state, proof.ok_or_else(Error::empty_response_proof)?))
    }

    fn proven_connection(
        &self,
        connection_id: &ConnectionId,
        height: ICSHeight,
    ) -> Result<(ConnectionEnd, MerkleProof), Error> {
        let key = storage::connection_key(connection_id);
        let (value, proof) = self.query(key, Some(height), true)?;
        let connection_end = ConnectionEnd::decode_vec(&value).map_err(Error::decode)?;

        Ok((
            connection_end,
            proof.ok_or_else(Error::empty_response_proof)?,
        ))
    }

    fn proven_client_consensus(
        &self,
        client_id: &ClientId,
        consensus_height: ICSHeight,
        height: ICSHeight,
    ) -> Result<(AnyConsensusState, MerkleProof), Error> {
        let key = storage::consensus_state_key(client_id, consensus_height);
        let (value, proof) = self.query(key, Some(height), true)?;
        let consensus_state = AnyConsensusState::decode_vec(&value).map_err(Error::decode)?;

        Ok((
            consensus_state,
            proof.ok_or_else(Error::empty_response_proof)?,
        ))
    }

    fn proven_channel(
        &self,
        port_id: &PortId,
        channel_id: &ChannelId,
        height: ICSHeight,
    ) -> Result<(ChannelEnd, MerkleProof), Error> {
        let port_channel_id = PortChannelId {
            port_id: port_id.clone(),
            channel_id: channel_id.clone(),
        };
        let key = storage::channel_key(&port_channel_id);
        let (value, proof) = self.query(key, Some(height), true)?;
        let channel_end = ChannelEnd::decode_vec(&value).map_err(Error::decode)?;

        Ok((channel_end, proof.ok_or_else(Error::empty_response_proof)?))
    }

    fn proven_packet(
        &self,
        packet_type: PacketMsgType,
        port_id: PortId,
        channel_id: ChannelId,
        sequence: Sequence,
        height: ICSHeight,
    ) -> Result<(Vec<u8>, MerkleProof), Error> {
        let key = match packet_type {
            PacketMsgType::Recv => storage::commitment_key(&port_id, &channel_id, sequence),
            PacketMsgType::Ack => storage::ack_key(&port_id, &channel_id, sequence),
            PacketMsgType::TimeoutUnordered | PacketMsgType::TimeoutOnClose => {
                storage::receipt_key(&port_id, &channel_id, sequence)
            }
            PacketMsgType::TimeoutOrdered => {
                let port_channel_id = PortChannelId {
                    port_id,
                    channel_id,
                };
                storage::next_sequence_recv_key(&port_channel_id)
            }
        };
        let (data, proof) = self.query(key, Some(height), true)?;
        Ok((data, proof.ok_or_else(Error::empty_response_proof)?))
    }

    fn build_client_state(
        &self,
        height: ICSHeight,
        settings: ClientSettings,
    ) -> Result<Self::ClientState, Error> {
        let ClientSettings::Tendermint(settings) = settings;
        // TODO set unbonding_period
        let unbonding_period = Duration::new(1814400, 0);
        let trusting_period = settings.trusting_period.unwrap_or_else(|| {
            self.config
                .trusting_period
                .unwrap_or(2 * unbonding_period / 3)
        });
        // TODO confirm parameters for Anoma
        ClientState::new(
            self.id().clone(),
            self.config.trust_threshold.into(),
            trusting_period,
            unbonding_period,
            settings.max_clock_drift,
            height,
            self.config.proof_specs.clone(),
            vec!["upgrade".to_string(), "upgradedIBCState".to_string()],
            AllowUpdate {
                after_expiry: true,
                after_misbehaviour: true,
            },
        )
        .map_err(Error::ics07)
    }

    fn build_consensus_state(
        &self,
        light_block: Self::LightBlock,
    ) -> Result<Self::ConsensusState, Error> {
        Ok(TMConsensusState::from(light_block.signed_header.header))
    }

    fn build_header(
        &self,
        trusted_height: ICSHeight,
        target_height: ICSHeight,
        client_state: &AnyClientState,
        light_client: &mut Self::LightClient,
    ) -> Result<(Self::Header, Vec<Self::Header>), Error> {
        // Get the light block at target_height from chain.
        let Verified { target, supporting } =
            light_client.header_and_minimal_set(trusted_height, target_height, client_state)?;

        Ok((target, supporting))
    }

    fn ibc_version(&self) -> Result<Option<semver::Version>, Error> {
        unimplemented!()
    }
}

/// TODO make it public in Anoma
/// Returns a key of the IBC-related data
fn ibc_key(path: impl AsRef<str>) -> Result<Key, Error> {
    let path = Key::parse(path).map_err(|e| Error::query(e.to_string()))?;
    let addr = Address::Internal(InternalAddress::Ibc);
    let key = Key::from(addr.to_db_key());
    Ok(key.join(&path))
}
