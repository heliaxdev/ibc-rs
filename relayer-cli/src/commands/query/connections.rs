use abscissa_core::clap::Parser;
use abscissa_core::Runnable;

use ibc::core::ics24_host::identifier::{ChainId, ConnectionId};
use ibc_proto::ibc::core::connection::v1::QueryConnectionsRequest;
use ibc_relayer::chain::handle::{BaseChainHandle, ChainHandle};

use crate::conclude::Output;
use crate::prelude::*;

#[derive(Clone, Command, Debug, Parser)]
pub struct QueryConnectionsCmd {
    #[clap(required = true, help = "identifier of the chain to query")]
    chain_id: ChainId,
}

// hermes query connections ibc-0
impl Runnable for QueryConnectionsCmd {
    fn run(&self) {
        debug!("Options: {:?}", self);

        let chain = super::get_chain_handle::<BaseChainHandle>(&self.chain_id);

        let req = QueryConnectionsRequest {
            pagination: ibc_proto::cosmos::base::query::pagination::all(),
        };

        let res = chain.query_connections(req);

        match res {
            Ok(connections) => {
                let ids: Vec<ConnectionId> = connections
                    .into_iter()
                    .map(|identified_connection| identified_connection.connection_id)
                    .collect();

                Output::success(ids).exit()
            }
            Err(e) => Output::error(format!("{}", e)).exit(),
        }
    }
}
