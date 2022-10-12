use abscissa_core::clap::Parser;
use abscissa_core::{Command, Runnable};

use ibc::core::ics24_host::identifier::ChainId;
use ibc::core::ics24_host::identifier::{ChannelId, PortId};
use ibc_relayer::chain::handle::{BaseChainHandle, ChainHandle};

use crate::conclude::Output;
use crate::prelude::*;
use ibc::core::ics04_channel::channel::State;

#[derive(Clone, Command, Debug, Parser)]
pub struct QueryChannelEndCmd {
    #[clap(required = true, help = "identifier of the chain to query")]
    chain_id: ChainId,

    #[clap(required = true, help = "identifier of the port to query")]
    port_id: PortId,

    #[clap(required = true, help = "identifier of the channel to query")]
    channel_id: ChannelId,

    #[clap(short = 'H', long, help = "height of the state to query")]
    height: Option<u64>,
}

impl Runnable for QueryChannelEndCmd {
    fn run(&self) {
        debug!("Options: {:?}", self);

        let chain = super::get_chain_handle::<BaseChainHandle>(&self.chain_id);

        let height = ibc::Height::new(chain.id().version(), self.height.unwrap_or(0_u64));
        let res = chain.query_channel(&self.port_id, &self.channel_id, height);
        match res {
            Ok(channel_end) => {
                if channel_end.state_matches(&State::Uninitialized) {
                    Output::error(format!(
                        "port '{}' & channel '{}' does not exist",
                        self.port_id, self.channel_id
                    ))
                    .exit()
                } else {
                    Output::success(channel_end).exit()
                }
            }
            Err(e) => Output::error(format!("{}", e)).exit(),
        }
    }
}
