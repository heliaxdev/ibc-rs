use alloc::sync::Arc;
use core::str::FromStr;

use core::time::Duration;
use std::path::Path;
use std::thread;

use ibc::clients::ics07_tendermint::client_state::{AllowUpdate, ClientState};
use ibc::clients::ics07_tendermint::consensus_state::ConsensusState as TMConsensusState;
use ibc::clients::ics07_tendermint::header::Header as TmHeader;
use ibc::core::ics02_client::client_consensus::{AnyConsensusState, AnyConsensusStateWithHeight};
use ibc::core::ics02_client::client_state::IdentifiedAnyClientState;
use ibc::core::ics02_client::client_state::{AnyClientState, ClientState as Ics02ClientState};
use ibc::core::ics03_connection::connection::{ConnectionEnd, IdentifiedConnectionEnd};
use ibc::core::ics04_channel::channel::{ChannelEnd, IdentifiedChannelEnd};
use ibc::core::ics04_channel::packet::{PacketMsgType, Sequence};
use ibc::core::ics23_commitment::commitment::CommitmentPrefix;
use ibc::core::ics24_host::identifier::{ChainId, ChannelId, ClientId, ConnectionId, PortId};
use ibc::events::IbcEvent;
use ibc::query::QueryBlockRequest;
use ibc::query::QueryTxRequest;
use ibc::signer::Signer;
use ibc::Height as ICSHeight;
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
use namada::ibc::core::ics04_channel::packet::Sequence as NamadaSequence;
use namada::ibc::core::ics24_host::identifier::{
    ChannelId as NamadaChannelId, ClientId as NamadaClientId, ConnectionId as NamadaConnectionId,
    PortChannelId as NamadaPortChannelId, PortId as NamadaPortId,
};
use namada::ibc::Height as NamadaIcsHeight;
use namada::ledger::ibc::handler::commitment_prefix;
use namada::ledger::ibc::storage;
use namada::ledger::storage::ics23_specs::ibc_proof_specs;
use namada::ledger::storage::Sha256Hasher;
use namada::tendermint::abci::Event as NamadaTmEvent;
use namada::tendermint::block::Height;
use namada::tendermint_proto::Protobuf as AbciPlusProtobuf;
use namada::types::address::{Address, InternalAddress};
use namada::types::storage::{Key, KeySeg, PrefixValue};
use namada_apps::wallet::Wallet;
use prost::Message;
use tendermint::abci::transaction::Hash;
use tendermint::Time;
use tendermint_light_client::types::LightBlock as TMLightBlock;
use tendermint_light_client::types::PeerId;
use tendermint_proto::Protobuf;
use tendermint_rpc::endpoint::broadcast::tx_sync::Response;
use tendermint_rpc::endpoint::tx::Response as TxResponse;
use tendermint_rpc::query::Query;
use tendermint_rpc_abciplus::endpoint::tx::Response as AbciPlusTxResponse;
use tendermint_rpc_abciplus::query::{EventType as AbciPlusEventType, Query as AbciPlusQuery};
use tendermint_rpc_abciplus::{Client, HttpClient, Order, Url};
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

const BASE_WALLET_DIR: &str = "namada_wallet";

pub mod query;
pub mod tx;

pub struct NamadaChain {
    config: ChainConfig,
    rpc_client: HttpClient,
    rt: Arc<TokioRuntime>,
    keybase: KeyRing,
}

impl ChainEndpoint for NamadaChain {
    type LightBlock = TMLightBlock;
    type Header = TmHeader;
    type ConsensusState = TMConsensusState;
    type ClientState = ClientState;
    type LightClient = TmLightClient;

    fn bootstrap(config: ChainConfig, rt: Arc<TokioRuntime>) -> Result<Self, Error> {
        let rpc_addr = Url::from_str(&config.rpc_addr.to_string()).unwrap();
        let rpc_client = HttpClient::new(rpc_addr)
            .map_err(|e| Error::abci_plus_rpc(config.rpc_addr.clone(), e))?;

        // not used in Anoma, but the trait requires KeyRing
        let keybase = KeyRing::new(config.key_store_type, &config.account_prefix, &config.id)
            .map_err(Error::key_base)?;

        // check if the wallet has been set up for this relayer
        let wallet_path = Path::new(BASE_WALLET_DIR).join(config.id.to_string());
        let mut wallet =
            Wallet::load(&wallet_path).ok_or_else(Error::namada_wallet_not_initialized)?;
        wallet
            .find_key(&config.key_name)
            .map_err(Error::namada_key_pair_not_found)?;

        // overwrite the proof spec
        // TODO: query the proof spec
        let config = ChainConfig {
            proof_specs: ibc_proof_specs::<Sha256Hasher>().into(),
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
        let peer_id = self
            .rt
            .block_on(self.rpc_client.status())
            .map(|s| PeerId::from_str(&s.node_info.id.to_string()).unwrap())
            .map_err(|e| Error::abci_plus_rpc(self.config.rpc_addr.clone(), e))?;

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
            Error::abci_plus_health_check_json_rpc(
                self.config.id.clone(),
                self.config.rpc_addr.to_string(),
                "/health".to_string(),
                e,
            )
        })?;

        // TODO Namada health check

        self.rt
            .block_on(self.rpc_client.tx_search(
                AbciPlusQuery::from(AbciPlusEventType::NewBlock),
                false,
                1,
                1,
                Order::Ascending,
            ))
            .map_err(|e| {
                Error::abci_plus_health_check_json_rpc(
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
            .flat_map(|el| el.events)
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
        let wallet_path = Path::new(BASE_WALLET_DIR).join(self.config.id.to_string());
        let wallet = Wallet::load(&wallet_path).expect("wallet has not been initialized yet");
        let address = wallet
            .find_address(&self.config.key_name)
            .ok_or_else(|| Error::namada_address_not_found(self.config.key_name.clone()))?;

        Ok(Signer::new(address))
    }

    fn config(&self) -> ChainConfig {
        self.config.clone()
    }

    fn get_key(&mut self) -> Result<KeyEntry, Error> {
        unreachable!("this chain should use its wallet")
    }

    fn add_key(&mut self, _key_name: &str, _key: KeyEntry) -> Result<(), Error> {
        unreachable!("this chain should use its wallet")
    }

    fn query_commitment_prefix(&self) -> Result<CommitmentPrefix, Error> {
        Ok(commitment_prefix().into_vec().try_into().unwrap())
    }

    fn query_status(&self) -> Result<ChainStatus, Error> {
        let status = self
            .rt
            .block_on(self.rpc_client.status())
            .map_err(|e| Error::abci_plus_rpc(self.config.rpc_addr.clone(), e))?;

        if status.sync_info.catching_up {
            return Err(Error::chain_not_caught_up(
                self.config.rpc_addr.to_string(),
                self.config().id,
            ));
        }

        let time = Time::from_str(&status.sync_info.latest_block_time.to_string()).unwrap();
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
                let client_id = ClientId::from_str(&client_id.to_string()).unwrap();
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
        let client_id = NamadaClientId::from_str(&client_id.to_string()).unwrap();
        let key = storage::client_state_key(&client_id);
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
                height: ICSHeight::new(height.revision_number, height.revision_height),
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
        let client_id = NamadaClientId::from_str(&client_id.to_string()).unwrap();
        let consensus_height = NamadaIcsHeight::new(
            consensus_height.revision_number,
            consensus_height.revision_height,
        );
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
            let conn_id = storage::connection_id(&key).map_err(|e| Error::query(e.to_string()))?;
            let connection_id = ConnectionId::from_str(&conn_id.to_string()).unwrap();
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
        let connection_id = NamadaConnectionId::from_str(&connection_id.to_string()).unwrap();
        let key = storage::connection_key(&connection_id);
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
            let port_id = PortId::from_str(&port_channel_id.port_id.to_string()).unwrap();
            let channel_id = ChannelId::from_str(&port_channel_id.channel_id.to_string()).unwrap();
            let channel = ChannelEnd::decode_vec(&value).map_err(Error::decode)?;
            channels.push(IdentifiedChannelEnd::new(port_id, channel_id, channel))
        }

        Ok(channels)
    }

    fn query_channel(
        &self,
        port_id: &PortId,
        channel_id: &ChannelId,
        height: ICSHeight,
    ) -> Result<ChannelEnd, Error> {
        let port_channel_id = NamadaPortChannelId {
            port_id: NamadaPortId::from_str(port_id.as_ref()).unwrap(),
            channel_id: NamadaChannelId::from_str(&channel_id.to_string()).unwrap(),
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
        let port_id = NamadaPortId::from_str(&request.port_id)
            .map_err(|_| Error::query(format!("invalid port ID {}", request.port_id)))?;
        let channel_id = NamadaChannelId::from_str(&request.channel_id)
            .map_err(|_| Error::query(format!("invalid channel ID {}", request.channel_id)))?;
        let port_channel_id = NamadaPortChannelId {
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
                let query =
                    AbciPlusQuery::from_str(&cosmos::query::header_query(&request).to_string())
                        .unwrap();
                let mut response = self
                    .rt
                    .block_on(self.rpc_client.tx_search(
                        query,
                        false,
                        1,
                        1, // get only the first Tx matching the query
                        Order::Ascending,
                    ))
                    .map_err(|e| Error::abci_plus_rpc(self.config.rpc_addr.clone(), e))?;

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
                    into_tx_response(tx),
                );

                Ok(event.into_iter().collect())
            }

            QueryTxRequest::Transaction(tx) => {
                let query = Query::default().and_eq("applied.hash", tx.0.to_string());
                let events = self.query_events(query)?;
                Ok(events)
            }
        }
    }

    fn query_blocks(
        &self,
        request: QueryBlockRequest,
    ) -> Result<(Vec<IbcEvent>, Vec<IbcEvent>), Error> {
        match request {
            QueryBlockRequest::Packet(request) => {
                crate::time!("query_blocks: query block packet events");

                let mut begin_block_events: Vec<IbcEvent> = vec![];
                let mut end_block_events: Vec<IbcEvent> = vec![];

                for seq in &request.sequences {
                    let query = AbciPlusQuery::from_str(
                        &cosmos::query::packet_query(&request, *seq).to_string(),
                    )
                    .unwrap();
                    let response = self
                        .rt
                        .block_on(self.rpc_client.block_search(
                            query,
                            1,
                            1, // there should only be a single match for this query
                            Order::Ascending,
                        ))
                        .map_err(|e| Error::abci_plus_rpc(self.config.rpc_addr.clone(), e))?;

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
                            .map_err(|e| Error::abci_plus_rpc(self.config.rpc_addr.clone(), e))?;

                        begin_block_events.append(
                            &mut response
                                .begin_block_events
                                .unwrap_or_default()
                                .into_iter()
                                .filter_map(|ev| {
                                    cosmos::filter_matching_event(into_event(ev), &request, *seq)
                                })
                                .collect(),
                        );

                        end_block_events.append(
                            &mut response
                                .end_block_events
                                .unwrap_or_default()
                                .into_iter()
                                .filter_map(|ev| {
                                    cosmos::filter_matching_event(into_event(ev), &request, *seq)
                                })
                                .collect(),
                        );
                    }
                }
                Ok((begin_block_events, end_block_events))
            }
        }
    }

    fn query_host_consensus_state(&self, height: ICSHeight) -> Result<Self::ConsensusState, Error> {
        let height =
            Height::try_from(height.revision_height).map_err(Error::abci_plus_invalid_height)?;

        // TODO(hu55a1n1): use the `/header` RPC endpoint instead when we move to tendermint v0.35.x
        let rpc_call = match height.value() {
            0 => self.rpc_client.latest_block(),
            _ => self.rpc_client.block(height),
        };
        let response = self
            .rt
            .block_on(rpc_call)
            .map_err(|e| Error::abci_plus_rpc(self.config.rpc_addr.clone(), e))?;
        let cs = namada::ibc::clients::ics07_tendermint::consensus_state::ConsensusState::from(
            response.block.header,
        );
        let consensus_state = TMConsensusState::decode_vec(&cs.encode_vec().unwrap()).unwrap();
        Ok(consensus_state)
    }

    fn proven_client_state(
        &self,
        client_id: &ClientId,
        height: ICSHeight,
    ) -> Result<(AnyClientState, MerkleProof), Error> {
        let client_id = NamadaClientId::from_str(&client_id.to_string()).unwrap();
        let key = storage::client_state_key(&client_id);
        let (value, proof) = self.query(key, Some(height), true)?;
        let client_state = AnyClientState::decode_vec(&value).map_err(Error::decode)?;

        Ok((client_state, proof.ok_or_else(Error::empty_response_proof)?))
    }

    fn proven_connection(
        &self,
        connection_id: &ConnectionId,
        height: ICSHeight,
    ) -> Result<(ConnectionEnd, MerkleProof), Error> {
        let connection_id = NamadaConnectionId::from_str(&connection_id.to_string()).unwrap();
        let key = storage::connection_key(&connection_id);
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
        let client_id = NamadaClientId::from_str(&client_id.to_string()).unwrap();
        let consensus_height = NamadaIcsHeight::new(
            consensus_height.revision_number,
            consensus_height.revision_height,
        );
        let key = storage::consensus_state_key(&client_id, consensus_height);
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
        let port_channel_id = NamadaPortChannelId {
            port_id: NamadaPortId::from_str(port_id.as_ref()).unwrap(),
            channel_id: NamadaChannelId::from_str(&channel_id.to_string()).unwrap(),
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
        let port_id = NamadaPortId::from_str(&port_id.to_string()).unwrap();
        let channel_id = NamadaChannelId::from_str(&channel_id.to_string()).unwrap();
        let sequence = NamadaSequence::from(u64::from(sequence));
        let key = match packet_type {
            PacketMsgType::Recv => storage::commitment_key(&port_id, &channel_id, sequence),
            PacketMsgType::Ack => storage::ack_key(&port_id, &channel_id, sequence),
            PacketMsgType::TimeoutUnordered | PacketMsgType::TimeoutOnClose => {
                storage::receipt_key(&port_id, &channel_id, sequence)
            }
            PacketMsgType::TimeoutOrdered => {
                let port_channel_id = NamadaPortChannelId {
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

/// Convert a transaction response to one of the base Tendermint
fn into_tx_response(resp: AbciPlusTxResponse) -> TxResponse {
    TxResponse {
        hash: Hash::from_str(&resp.hash.to_string()).unwrap(),
        height: u64::from(resp.height).try_into().unwrap(),
        index: resp.index,
        tx_result: tendermint::abci::responses::DeliverTx {
            code: u32::from(resp.tx_result.code).into(),
            data: Vec::<u8>::from(resp.tx_result.data).into(),
            log: tendermint::abci::Log::from(resp.tx_result.log.as_ref()),
            // not used
            info: tendermint::abci::Info::default(),
            gas_wanted: u64::from(resp.tx_result.gas_wanted).into(),
            gas_used: u64::from(resp.tx_result.gas_used).into(),
            events: resp.tx_result.events.into_iter().map(into_event).collect(),
            // not used
            codespace: tendermint::abci::responses::Codespace::default(),
        },
        tx: Vec::<u8>::from(resp.tx).into(),
        proof: resp.proof.map(|p| {
            let buf = prost::Message::encode_to_vec(&p);
            tendermint_proto::types::TxProof::decode(buf.as_slice()).unwrap()
        }),
    }
}

/// Convert a Tendermint event to one of the base Tendermint
fn into_event(event: NamadaTmEvent) -> tendermint::abci::Event {
    use tendermint::abci::tag::{Key, Tag, Value};
    use tendermint::abci::Event;

    Event {
        type_str: event.type_str,
        attributes: event
            .attributes
            .iter()
            .map(|tag| Tag {
                key: Key::from_str(&tag.key.to_string()).unwrap(),
                value: Value::from_str(&tag.value.to_string()).unwrap(),
            })
            .collect(),
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
