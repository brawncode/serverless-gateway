use ethers::types::U256;
use lazy_static::lazy_static;

pub const REQUEST_RELAY_TIMEOUT: u64 = 40;

// pub const RESPONSE_RELAY_TIMEOUT: u64 = 40;
pub const MAX_GATEWAY_RETRIES: u8 = 2;
pub const MAX_TX_RECEIPT_RETRIES: u8 = 5;
pub const MAX_RETRY_ON_PROVIDER_ERROR: u8 = 5;

pub const GATEWAY_BLOCK_STATES_TO_MAINTAIN: u64 = 5;
pub const WAIT_BEFORE_CHECKING_BLOCK: u64 = 5;

lazy_static! {
    pub static ref MIN_GATEWAY_STAKE: U256 = U256::from(111_111_111_111_111_110_000 as u128);
    pub static ref GATEWAY_STAKE_ADJUSTMENT_FACTOR: U256 = U256::from(1e18 as u128);
}
