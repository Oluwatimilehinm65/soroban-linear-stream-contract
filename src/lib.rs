//! Dripswave Stream — a linear token-streaming (vesting) contract for Soroban.
//!
//! A `sender` deposits a fixed amount of a token up front, naming a
//! `recipient` and a `[start, end]` time window. Tokens vest linearly across
//! that window. The recipient can withdraw any vested-but-unwithdrawn
//! balance at any time. The sender can cancel a stream at any time; vested
//! funds go to the recipient and the unvested remainder is refunded to the
//! sender. Vesting is computed lazily from ledger timestamps — no external
//! keeper or cron is required.
#![no_std]

use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, token, Address, Env,
};

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Stream {
    pub sender: Address,
    pub recipient: Address,
    pub token: Address,
    /// Total amount deposited when the stream was created. Immutable.
    pub deposit: i128,
    /// Unix timestamp (seconds) at which vesting begins.
    pub start_time: u64,
    /// Unix timestamp (seconds) at which vesting completes.
    pub end_time: u64,
    /// Cumulative amount already withdrawn by the recipient.
    pub withdrawn: i128,
    /// Set once `cancel` is called; vesting is frozen at `cancel_time`.
    pub canceled: bool,
    /// Ledger timestamp at which the stream was canceled (0 if never).
    pub cancel_time: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[contracttype]
pub enum DataKey {
    /// Monotonically increasing counter used to mint the next stream id.
    NextId,
    /// Individual stream, keyed by id.
    Stream(u32),
}

#[contracterror]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum StreamError {
    /// `end_time` was not strictly after `start_time`.
    InvalidTimeRange = 1,
    /// `deposit` was zero or negative.
    InvalidDeposit = 2,
    /// No stream exists for the given id.
    StreamNotFound = 3,
    /// The stream was already canceled.
    AlreadyCanceled = 4,
    /// The requested withdrawal exceeds the currently vested, unwithdrawn balance.
    InsufficientVestedBalance = 5,
    /// The requested withdrawal amount was zero or negative.
    InvalidWithdrawAmount = 6,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamCreated {
    #[topic]
    pub stream_id: u32,
    pub sender: Address,
    pub recipient: Address,
    pub token: Address,
    pub deposit: i128,
    pub start_time: u64,
    pub end_time: u64,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamWithdrawn {
    #[topic]
    pub stream_id: u32,
    pub recipient: Address,
    pub amount: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamCanceled {
    #[topic]
    pub stream_id: u32,
    pub sender: Address,
    pub recipient: Address,
    pub refunded_to_sender: i128,
    pub paid_to_recipient: i128,
}

#[contract]
pub struct DripswaveStream;

#[contractimpl]
impl DripswaveStream {
    /// Create a new stream. `sender` must authorize; `deposit` is pulled
    /// from `sender` into the contract's own token balance immediately, so
    /// the full amount is escrowed for the lifetime of the stream. Returns
    /// the new stream's id.
    pub fn create_stream(
        env: Env,
        sender: Address,
        recipient: Address,
        token: Address,
        deposit: i128,
        start_time: u64,
        end_time: u64,
    ) -> Result<u32, StreamError> {
        sender.require_auth();

        if end_time <= start_time {
            return Err(StreamError::InvalidTimeRange);
        }
        if deposit <= 0 {
            return Err(StreamError::InvalidDeposit);
        }

        let id = Self::next_id(&env);

        let stream = Stream {
            sender: sender.clone(),
            recipient: recipient.clone(),
            token: token.clone(),
            deposit,
            start_time,
            end_time,
            withdrawn: 0,
            canceled: false,
            cancel_time: 0,
        };
        env.storage().persistent().set(&DataKey::Stream(id), &stream);

        let token_client = token::Client::new(&env, &token);
        token_client.transfer(&sender, &env.current_contract_address(), &deposit);

        StreamCreated {
            stream_id: id,
            sender,
            recipient,
            token,
            deposit,
            start_time,
            end_time,
        }
        .publish(&env);

        Ok(id)
    }

    /// Amount currently vested but not yet withdrawn for `stream_id`, as of
    /// the current ledger timestamp (or the cancellation time, if canceled).
    pub fn withdrawable(env: Env, stream_id: u32) -> Result<i128, StreamError> {
        let stream = Self::load_stream(&env, stream_id)?;
        let now = env.ledger().timestamp();
        Ok(vested_amount(&stream, now) - stream.withdrawn)
    }

    /// Withdraw `amount` of the vested balance to the stream's recipient.
    /// The recipient must authorize. Reverts if `amount` exceeds what is
    /// currently vested and unwithdrawn.
    pub fn withdraw(env: Env, stream_id: u32, amount: i128) -> Result<(), StreamError> {
        if amount <= 0 {
            return Err(StreamError::InvalidWithdrawAmount);
        }

        let mut stream = Self::load_stream(&env, stream_id)?;
        stream.recipient.require_auth();

        let now = env.ledger().timestamp();
        let available = vested_amount(&stream, now) - stream.withdrawn;
        if amount > available {
            return Err(StreamError::InsufficientVestedBalance);
        }

        stream.withdrawn += amount;
        env.storage()
            .persistent()
            .set(&DataKey::Stream(stream_id), &stream);

        let token_client = token::Client::new(&env, &stream.token);
        token_client.transfer(
            &env.current_contract_address(),
            &stream.recipient,
            &amount,
        );

        StreamWithdrawn {
            stream_id,
            recipient: stream.recipient,
            amount,
        }
        .publish(&env);

        Ok(())
    }

    /// Cancel a stream. The sender must authorize. Whatever has vested (and
    /// not yet been withdrawn) as of the cancellation moment is paid to the
    /// recipient; the remainder is refunded to the sender. A stream can only
    /// be canceled once.
    pub fn cancel(env: Env, stream_id: u32) -> Result<(), StreamError> {
        let mut stream = Self::load_stream(&env, stream_id)?;
        stream.sender.require_auth();

        if stream.canceled {
            return Err(StreamError::AlreadyCanceled);
        }

        let now = env.ledger().timestamp();
        let vested = vested_amount(&stream, now);
        let owed_to_recipient = vested - stream.withdrawn;
        let owed_to_sender = stream.deposit - vested;

        stream.canceled = true;
        stream.cancel_time = now;
        stream.withdrawn = vested;
        env.storage()
            .persistent()
            .set(&DataKey::Stream(stream_id), &stream);

        let token_client = token::Client::new(&env, &stream.token);
        if owed_to_recipient > 0 {
            token_client.transfer(
                &env.current_contract_address(),
                &stream.recipient,
                &owed_to_recipient,
            );
        }
        if owed_to_sender > 0 {
            token_client.transfer(
                &env.current_contract_address(),
                &stream.sender,
                &owed_to_sender,
            );
        }

        StreamCanceled {
            stream_id,
            sender: stream.sender,
            recipient: stream.recipient,
            refunded_to_sender: owed_to_sender,
            paid_to_recipient: owed_to_recipient,
        }
        .publish(&env);

        Ok(())
    }

    /// Fetch a stream's full stored state.
    pub fn get_stream(env: Env, stream_id: u32) -> Result<Stream, StreamError> {
        Self::load_stream(&env, stream_id)
    }

    fn load_stream(env: &Env, stream_id: u32) -> Result<Stream, StreamError> {
        env.storage()
            .persistent()
            .get(&DataKey::Stream(stream_id))
            .ok_or(StreamError::StreamNotFound)
    }

    fn next_id(env: &Env) -> u32 {
        let id: u32 = env
            .storage()
            .instance()
            .get(&DataKey::NextId)
            .unwrap_or(0);
        env.storage().instance().set(&DataKey::NextId, &(id + 1));
        id
    }
}

/// Pure vesting math, shared by `withdrawable`, `withdraw`, and `cancel`.
/// Linear interpolation between `start_time` and `end_time`, clamped to
/// `[0, deposit]`. If the stream has been canceled, `now` is effectively
/// capped at `cancel_time` so vesting stops accruing from that point on.
fn vested_amount(stream: &Stream, now: u64) -> i128 {
    let effective_now = if stream.canceled {
        stream.cancel_time
    } else {
        now
    };

    if effective_now <= stream.start_time {
        return 0;
    }
    if effective_now >= stream.end_time {
        return stream.deposit;
    }

    let elapsed = (effective_now - stream.start_time) as i128;
    let total = (stream.end_time - stream.start_time) as i128;
    // Integer division rounds down: the recipient never receives more than
    // has strictly vested, and any dust from rounding stays escrowed until
    // it either vests fully or is refunded to the sender on cancellation.
    stream.deposit * elapsed / total
}

#[cfg(test)]
mod test;