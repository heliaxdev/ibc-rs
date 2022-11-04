use core::str::FromStr;

use ibc::events::IbcEvent;
use ibc::Height as ICSHeight;
use ibc_proto::ibc::core::commitment::v1::MerkleProof;
use namada::ibc::core::ics23_commitment::merkle::convert_tm_to_ics_merkle_proof;
use namada::ibc::events::{from_tx_response_event, IbcEvent as NamadaIbcEvent};
use namada::ibc::Height as NamadaIcsHeight;
use namada::ledger::queries::RPC;
use namada::tendermint::abci::tag::Tag;
use namada::tendermint::abci::Event as NamadaTmEvent;
use namada::types::storage::{BlockHeight, Epoch, Key, PrefixValue};
use prost::Message;
use tendermint_rpc::query::Query;
use tendermint_rpc_abciplus::query::Query as AbciPlusQuery;
use tendermint_rpc_abciplus::{Client, Order};

use crate::error::Error;

use super::super::ChainEndpoint;
use super::NamadaChain;

impl NamadaChain {
    pub fn query(
        &self,
        key: Key,
        height: Option<ICSHeight>,
        prove: bool,
    ) -> Result<(Vec<u8>, Option<MerkleProof>), Error> {
        let height = height
            .map(|h| BlockHeight::try_from(h.revision_height).expect("height conversion failed"));
        let response = self
            .rt
            .block_on(
                RPC.shell()
                    .storage_value(&self.rpc_client, None, height, prove, &key),
            )
            .map_err(Error::namada_query)?;

        let proof = if prove {
            let p = response.proof.ok_or_else(Error::empty_response_proof)?;
            let mp = convert_tm_to_ics_merkle_proof(&p).map_err(Error::abci_plus_ics23)?;
            // convert MerkleProof to one of the base tendermint
            let buf = prost::Message::encode_to_vec(&mp);
            let proof = MerkleProof::decode(buf.as_slice()).unwrap();
            Some(proof)
        } else {
            None
        };

        Ok((response.data, proof))
    }

    pub fn query_prefix(&self, prefix: Key) -> Result<Vec<PrefixValue>, Error> {
        let response = self
            .rt
            .block_on(
                RPC.shell()
                    .storage_prefix(&self.rpc_client, None, None, false, &prefix),
            )
            .map_err(Error::namada_query)?;
        Ok(response.data)
    }

    pub fn query_epoch(&self) -> Result<Epoch, Error> {
        self.rt
            .block_on(RPC.shell().epoch(&self.rpc_client))
            .map_err(Error::namada_query)
    }

    pub fn query_events(&self, query: Query) -> Result<Vec<IbcEvent>, Error> {
        let query = AbciPlusQuery::from_str(&query.to_string()).unwrap();
        let blocks = &self
            .rt
            .block_on(self.rpc_client.block_search(query, 1, 1, Order::Ascending))
            .map_err(|e| Error::abci_plus_rpc(self.config.rpc_addr.clone(), e))?
            .blocks;
        let block = &blocks
            .get(0)
            .ok_or_else(|| Error::query("No block was found".to_string()))?
            .block;
        let response = self
            .rt
            .block_on(self.rpc_client.block_results(block.header.height))
            .map_err(|e| Error::abci_plus_rpc(self.config.rpc_addr.clone(), e))?;

        let events = response
            .end_block_events
            .ok_or_else(|| Error::query("No transaction result was found".to_string()))?;
        let mut ibc_events = vec![];
        for event in &events {
            let height = NamadaIcsHeight::new(self.id().version(), u64::from(response.height));
            match from_tx_response_event(height, event) {
                Some(e) => ibc_events.push(into_ibc_event(e)),
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

/// Convert an IbcEvent to one of the base Tendermint
fn into_ibc_event(event: NamadaIbcEvent) -> IbcEvent {
    let height = event.height();
    let height = ibc::Height::new(height.revision_number, height.revision_height);
    let namada_abci_event = NamadaTmEvent::try_from(event).unwrap();
    let abci_event = super::into_event(namada_abci_event);
    // The event should exist
    ibc::events::from_tx_response_event(height, &abci_event).unwrap()
}
