#![allow(dead_code)]

use crate::prelude::*;
use tendermint::{block, consensus, duration::Duration, evidence, public_key::Algorithm};

use crate::signer::Signer;
use tendermint::consensus::params::{TimeoutParams, SynchronyParams};

// Needed in mocks.
pub fn default_consensus_params() -> consensus::Params {
    consensus::Params {
        block: block::Size {
            max_bytes: 22020096,
            max_gas: -1,
        },
        evidence: evidence::Params {
            max_age_num_blocks: 100000,
            max_age_duration: Duration::new(48 * 3600, 0),
            max_bytes: 0,
        },
        validator: consensus::params::ValidatorParams {
            pub_key_types: vec![Algorithm::Ed25519],
        },
        version: Some(consensus::params::VersionParams::default()),
        synchrony: SynchronyParams {
            message_delay: Duration::from_millis(505),
            precision: Duration::from_secs(12),
        },
        timeout: TimeoutParams {
            propose: Duration::from_millis(3000),
            propose_delta: Duration::from_millis(500),
            vote: Duration::from_millis(1000),
            vote_delta: Duration::from_millis(500),
            commit: Duration::from_millis(1000),
            bypass_commit_timeout: false,
        },
    }
}

pub fn get_dummy_proof() -> Vec<u8> {
    "Y29uc2Vuc3VzU3RhdGUvaWJjb25lY2xpZW50LzIy"
        .as_bytes()
        .to_vec()
}

pub fn get_dummy_account_id() -> Signer {
    "0CDA3F47EF3C4906693B170EF650EB968C5F4B2C".parse().unwrap()
}

pub fn get_dummy_bech32_account() -> String {
    "cosmos1wxeyh7zgn4tctjzs0vtqpc6p5cxq5t2muzl7ng".to_string()
}
