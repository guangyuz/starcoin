// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    account_address::AccountAddress,
    block_metadata::BlockMetadata,
    contract_event::ContractEvent,
    vm_error::{StatusCode, StatusType, VMStatus},
    write_set::WriteSet,
};
use anyhow::{ensure, format_err, Error, Result};
use crypto::{ed25519::*, hash::CryptoHash, traits::*, HashValue};

use serde::{de, ser, Deserialize, Serialize};
use std::{
    collections::HashMap,
    convert::{TryFrom, TryInto},
    fmt,
    time::Duration,
};

mod change_set;
pub mod helpers;
mod module;
mod script;
mod transaction_argument;

pub use change_set::ChangeSet;
pub use module::Module;
pub use script::{Script, SCRIPT_HASH_LENGTH};

use std::ops::Deref;
pub use transaction_argument::{parse_as_transaction_argument, TransactionArgument};

pub type Version = u64; // Height - also used for MVCC in StateDB

pub const MAX_TRANSACTION_SIZE_IN_BYTES: usize = 4096;

/// RawUserTransaction is the portion of a transaction that a client signs
#[derive(Clone, Debug, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct RawUserTransaction {
    /// Sender's address.
    sender: AccountAddress,
    // Sequence number of this transaction corresponding to sender's account.
    sequence_number: u64,
    // The transaction script to execute.
    payload: TransactionPayload,

    // Maximal total gas specified by wallet to spend for this transaction.
    max_gas_amount: u64,
    // Maximal price can be paid per gas.
    gas_unit_price: u64,
    // Expiration time for this transaction.  If storage is queried and
    // the time returned is greater than or equal to this time and this
    // transaction has not been included, you can be certain that it will
    // never be included.
    // A transaction that doesn't expire is represented by a very large value like
    // u64::max_value().
    #[serde(serialize_with = "serialize_duration")]
    #[serde(deserialize_with = "deserialize_duration")]
    expiration_time: Duration,
}

// TODO(#1307)
fn serialize_duration<S>(d: &Duration, serializer: S) -> std::result::Result<S::Ok, S::Error>
where
    S: ser::Serializer,
{
    serializer.serialize_u64(d.as_secs())
}

fn deserialize_duration<'de, D>(deserializer: D) -> std::result::Result<Duration, D::Error>
where
    D: de::Deserializer<'de>,
{
    struct DurationVisitor;
    impl<'de> de::Visitor<'de> for DurationVisitor {
        type Value = Duration;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("Duration as u64")
        }

        fn visit_u64<E>(self, v: u64) -> std::result::Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(Duration::from_secs(v))
        }
    }

    deserializer.deserialize_u64(DurationVisitor)
}

impl RawUserTransaction {
    /// Create a new `RawUserTransaction` with a payload.
    ///
    /// It can be either to publish a module, to execute a script, or to issue a writeset
    /// transaction.
    pub fn new(
        sender: AccountAddress,
        sequence_number: u64,
        payload: TransactionPayload,
        max_gas_amount: u64,
        gas_unit_price: u64,
        expiration_time: Duration,
    ) -> Self {
        RawUserTransaction {
            sender,
            sequence_number,
            payload,
            max_gas_amount,
            gas_unit_price,
            expiration_time,
        }
    }

    /// Create a new `RawUserTransaction` with a script.
    ///
    /// A script transaction contains only code to execute. No publishing is allowed in scripts.
    pub fn new_script(
        sender: AccountAddress,
        sequence_number: u64,
        script: Script,
        max_gas_amount: u64,
        gas_unit_price: u64,
        expiration_time: Duration,
    ) -> Self {
        RawUserTransaction {
            sender,
            sequence_number,
            payload: TransactionPayload::Script(script),
            max_gas_amount,
            gas_unit_price,
            expiration_time,
        }
    }

    /// Create a new `RawUserTransaction` with a module to publish.
    ///
    /// A module transaction is the only way to publish code. Only one module per transaction
    /// can be published.
    pub fn new_module(
        sender: AccountAddress,
        sequence_number: u64,
        module: Module,
        max_gas_amount: u64,
        gas_unit_price: u64,
        expiration_time: Duration,
    ) -> Self {
        RawUserTransaction {
            sender,
            sequence_number,
            payload: TransactionPayload::Module(module),
            max_gas_amount,
            gas_unit_price,
            expiration_time,
        }
    }

    /// Signs the given `RawUserTransaction`. Note that this consumes the `RawUserTransaction` and turns it
    /// into a `SignatureCheckedTransaction`.
    ///
    /// For a transaction that has just been signed, its signature is expected to be valid.
    pub fn sign(
        self,
        private_key: &Ed25519PrivateKey,
        public_key: Ed25519PublicKey,
    ) -> Result<SignatureCheckedTransaction> {
        let signature = private_key.sign_message(&self.crypto_hash());
        Ok(SignatureCheckedTransaction(SignedUserTransaction::new(
            self, public_key, signature,
        )))
    }

    pub fn into_payload(self) -> TransactionPayload {
        self.payload
    }

    pub fn format_for_client(&self, get_transaction_name: impl Fn(&[u8]) -> String) -> String {
        let empty_vec = vec![];
        let (code, args) = match &self.payload {
            TransactionPayload::Script(script) => {
                (get_transaction_name(script.code()), script.args())
            }
            TransactionPayload::Module(_) => ("module publishing".to_string(), &empty_vec[..]),
        };
        let mut f_args: String = "".to_string();
        for arg in args {
            f_args = format!("{}\n\t\t\t{:#?},", f_args, arg);
        }
        format!(
            "RawUserTransaction {{ \n\
             \tsender: {}, \n\
             \tsequence_number: {}, \n\
             \tpayload: {{, \n\
             \t\ttransaction: {}, \n\
             \t\targs: [ {} \n\
             \t\t]\n\
             \t}}, \n\
             \tmax_gas_amount: {}, \n\
             \tgas_unit_price: {}, \n\
             \texpiration_time: {:#?}, \n\
             }}",
            self.sender,
            self.sequence_number,
            code,
            f_args,
            self.max_gas_amount,
            self.gas_unit_price,
            self.expiration_time,
        )
    }
    /// Return the sender of this transaction.
    pub fn sender(&self) -> AccountAddress {
        self.sender
    }
}

#[derive(Clone, Debug, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub enum TransactionPayload {
    /// A transaction that executes code.
    Script(Script),
    /// A transaction that publishes code.
    Module(Module),
}

/// A transaction that has been signed.
///
/// A `SignedUserTransaction` is a single transaction that can be atomically executed. Clients submit
/// these to validator nodes, and the validator and executor submits these to the VM.
///
/// **IMPORTANT:** The signature of a `SignedUserTransaction` is not guaranteed to be verified. For a
/// transaction whose signature is statically guaranteed to be verified, see
/// [`SignatureCheckedTransaction`].
#[derive(Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct SignedUserTransaction {
    /// The raw transaction
    raw_txn: RawUserTransaction,

    /// Sender's public key. When checking the signature, we first need to check whether this key
    /// is indeed the pre-image of the pubkey hash stored under sender's account.
    public_key: Ed25519PublicKey,

    /// Signature of the transaction that correspond to the public key
    signature: Ed25519Signature,
}

/// A transaction for which the signature has been verified. Created by
/// [`SignedUserTransaction::check_signature`] and [`RawUserTransaction::sign`].
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct SignatureCheckedTransaction(SignedUserTransaction);

impl SignatureCheckedTransaction {
    /// Returns the `SignedUserTransaction` within.
    pub fn into_inner(self) -> SignedUserTransaction {
        self.0
    }

    /// Returns the `RawUserTransaction` within.
    pub fn into_raw_transaction(self) -> RawUserTransaction {
        self.0.into_raw_transaction()
    }
}

impl Deref for SignatureCheckedTransaction {
    type Target = SignedUserTransaction;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Debug for SignedUserTransaction {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "SignedUserTransaction {{ \n \
             {{ raw_txn: {:#?}, \n \
             public_key: {:#?}, \n \
             signature: {:#?}, \n \
             }} \n \
             }}",
            self.raw_txn, self.public_key, self.signature,
        )
    }
}

impl SignedUserTransaction {
    pub fn new(
        raw_txn: RawUserTransaction,
        public_key: Ed25519PublicKey,
        signature: Ed25519Signature,
    ) -> SignedUserTransaction {
        SignedUserTransaction {
            raw_txn,
            public_key,
            signature,
        }
    }

    pub fn public_key(&self) -> Ed25519PublicKey {
        self.public_key.clone()
    }

    pub fn signature(&self) -> Ed25519Signature {
        self.signature.clone()
    }

    pub fn sender(&self) -> AccountAddress {
        self.raw_txn.sender
    }

    pub fn into_raw_transaction(self) -> RawUserTransaction {
        self.raw_txn
    }

    pub fn sequence_number(&self) -> u64 {
        self.raw_txn.sequence_number
    }

    pub fn payload(&self) -> &TransactionPayload {
        &self.raw_txn.payload
    }

    pub fn max_gas_amount(&self) -> u64 {
        self.raw_txn.max_gas_amount
    }

    pub fn gas_unit_price(&self) -> u64 {
        self.raw_txn.gas_unit_price
    }

    pub fn expiration_time(&self) -> Duration {
        self.raw_txn.expiration_time
    }

    pub fn raw_txn_bytes_len(&self) -> usize {
        scs::to_bytes(&self.raw_txn)
            .expect("Unable to serialize RawUserTransaction")
            .len()
    }

    /// Checks that the signature of given transaction. Returns `Ok(SignatureCheckedTransaction)` if
    /// the signature is valid.
    pub fn check_signature(self) -> Result<SignatureCheckedTransaction> {
        self.public_key
            .verify_signature(&self.raw_txn.crypto_hash(), &self.signature)?;
        Ok(SignatureCheckedTransaction(self))
    }

    pub fn format_for_client(&self, get_transaction_name: impl Fn(&[u8]) -> String) -> String {
        format!(
            "SignedUserTransaction {{ \n \
             raw_txn: {}, \n \
             public_key: {:#?}, \n \
             signature: {:#?}, \n \
             }}",
            self.raw_txn.format_for_client(get_transaction_name),
            self.public_key,
            self.signature,
        )
    }
}

/// The status of executing a transaction. The VM decides whether or not we should `Keep` the
/// transaction output or `Discard` it based upon the execution of the transaction. We wrap these
/// decisions around a `VMStatus` that provides more detail on the final execution state of the VM.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransactionStatus {
    /// Discard the transaction output
    Discard(VMStatus),

    /// Keep the transaction output
    Keep(VMStatus),
}

impl TransactionStatus {
    pub fn vm_status(&self) -> &VMStatus {
        match self {
            TransactionStatus::Discard(vm_status) | TransactionStatus::Keep(vm_status) => vm_status,
        }
    }
}

impl From<VMStatus> for TransactionStatus {
    fn from(vm_status: VMStatus) -> Self {
        let should_discard = match vm_status.status_type() {
            // Any unknown error should be discarded
            StatusType::Unknown => true,
            // Any error that is a validation status (i.e. an error arising from the prologue)
            // causes the transaction to not be included.
            StatusType::Validation => true,
            // If the VM encountered an invalid internal state, we should discard the transaction.
            StatusType::InvariantViolation => true,
            // A transaction that publishes code that cannot be verified will be charged.
            StatusType::Verification => false,
            // Even if we are unable to decode the transaction, there should be a charge made to
            // that user's account for the gas fees related to decoding, running the prologue etc.
            StatusType::Deserialization => false,
            // Any error encountered during the execution of the transaction will charge gas.
            StatusType::Execution => false,
        };

        if should_discard {
            TransactionStatus::Discard(vm_status)
        } else {
            TransactionStatus::Keep(vm_status)
        }
    }
}

/// The output of executing a transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransactionOutput {
    /// The list of events emitted during this transaction.
    events: Vec<ContractEvent>,

    /// The amount of gas used during execution.
    gas_used: u64,

    /// The execution status.
    status: TransactionStatus,
}

impl TransactionOutput {
    pub fn new(events: Vec<ContractEvent>, gas_used: u64, status: TransactionStatus) -> Self {
        TransactionOutput {
            events,
            gas_used,
            status,
        }
    }

    pub fn events(&self) -> &[ContractEvent] {
        &self.events
    }

    pub fn gas_used(&self) -> u64 {
        self.gas_used
    }

    pub fn status(&self) -> &TransactionStatus {
        &self.status
    }
}

/// `TransactionInfo` is the object we store in the transaction accumulator. It consists of the
/// transaction as well as the execution result of this transaction.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TransactionInfo {
    /// The hash of this transaction.
    transaction_hash: HashValue,

    /// The root hash of Sparse Merkle Tree describing the world state at the end of this
    /// transaction.
    state_root_hash: HashValue,

    /// The root hash of Merkle Accumulator storing all events emitted during this transaction.
    event_root_hash: HashValue,

    /// The amount of gas used.
    gas_used: u64,

    /// The major status. This will provide the general error class. Note that this is not
    /// particularly high fidelity in the presence of sub statuses but, the major status does
    /// determine whether or not the transaction is applied to the global state or not.
    major_status: StatusCode,
}

impl TransactionInfo {
    /// Constructs a new `TransactionInfo` object using transaction hash, state root hash and event
    /// root hash.
    pub fn new(
        transaction_hash: HashValue,
        state_root_hash: HashValue,
        event_root_hash: HashValue,
        gas_used: u64,
        major_status: StatusCode,
    ) -> TransactionInfo {
        TransactionInfo {
            transaction_hash,
            state_root_hash,
            event_root_hash,
            gas_used,
            major_status,
        }
    }

    /// Returns the hash of this transaction.
    pub fn transaction_hash(&self) -> HashValue {
        self.transaction_hash
    }

    /// Returns root hash of Sparse Merkle Tree describing the world state at the end of this
    /// transaction.
    pub fn state_root_hash(&self) -> HashValue {
        self.state_root_hash
    }

    /// Returns the root hash of Merkle Accumulator storing all events emitted during this
    /// transaction.
    pub fn event_root_hash(&self) -> HashValue {
        self.event_root_hash
    }

    /// Returns the amount of gas used by this transaction.
    pub fn gas_used(&self) -> u64 {
        self.gas_used
    }

    pub fn major_status(&self) -> StatusCode {
        self.major_status
    }
}

/// `Transaction` will be the transaction type used internally in the libra node to represent the
/// transaction to be processed and persisted.
///
/// We suppress the clippy warning here as we would expect most of the transaction to be user
/// transaction.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Transaction {
    /// Transaction submitted by the user. e.g: P2P payment transaction, publishing module
    /// transaction, etc.
    UserTransaction(SignedUserTransaction),

    /// Transaction that applies a WriteSet to the current storage. This should be used for ONLY for
    /// genesis right now.
    WriteSet(ChangeSet),

    /// Transaction to update the block metadata resource at the beginning of a block.
    BlockMetadata(BlockMetadata),
}

impl Transaction {
    pub fn as_signed_user_txn(&self) -> Result<&SignedUserTransaction> {
        match self {
            Transaction::UserTransaction(txn) => Ok(txn),
            _ => Err(format_err!("Not a user transaction.")),
        }
    }

    pub fn format_for_client(&self, get_transaction_name: impl Fn(&[u8]) -> String) -> String {
        match self {
            Transaction::UserTransaction(user_txn) => {
                user_txn.format_for_client(get_transaction_name)
            }
            // TODO: display proper information for client
            Transaction::WriteSet(_write_set) => String::from("genesis"),
            // TODO: display proper information for client
            Transaction::BlockMetadata(_block_metadata) => String::from("block_metadata"),
        }
    }
}

impl TryFrom<Transaction> for SignedUserTransaction {
    type Error = Error;

    fn try_from(txn: Transaction) -> Result<Self> {
        match txn {
            Transaction::UserTransaction(txn) => Ok(txn),
            _ => Err(format_err!("Not a user transaction.")),
        }
    }
}