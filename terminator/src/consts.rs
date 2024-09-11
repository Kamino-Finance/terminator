use anchor_lang::{prelude::Pubkey, solana_program};
use static_pubkey::static_pubkey;

/// Canonical null pubkey. Prints out as "nu11111111111111111111111111111111111111111"
pub const NULL_PUBKEY: Pubkey = Pubkey::new_from_array([
    11, 193, 238, 216, 208, 116, 241, 195, 55, 212, 76, 22, 75, 202, 40, 216, 76, 206, 27, 169,
    138, 64, 177, 28, 19, 90, 156, 0, 0, 0, 0, 0,
]);

pub const MAX_ACCOUNTS_PER_TRANSACTION: usize = 64;
pub const MAX_EXTRA_ACCOUNTS_BUFFER: usize = 30; // the maximum size of the buffer we allow for the extra accounts; it is a very conservative value and with the current max number of accounts that can be locked in a tx this should be never reached
pub const EXTRA_ACCOUNTS_BUFFER: usize = 5; // the max_accounts limit in Jup API is not strict, so we need some tolerance
pub const WRAPPED_SOL_MINT: Pubkey = static_pubkey!("So11111111111111111111111111111111111111112");
