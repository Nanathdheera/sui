// Copyright (c) 2021, Facebook, Inc. and its affiliates
// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0
use super::{base_types::*, batch::*, committee::Committee, error::*, event::Event};
use crate::committee::{EpochId, StakeUnit};
use crate::crypto::{
    sha3_hash, AuthoritySignInfo, AuthoritySignInfoTrait, AuthoritySignature,
    AuthorityStrongQuorumSignInfo, Ed25519SuiSignature, EmptySignInfo, Signable, Signature,
    SignatureScheme, SuiAuthoritySignature, SuiSignature, SuiSignatureInner, ToFromBytes,
    VerificationObligation,
};
use crate::gas::GasCostSummary;
use crate::messages_checkpoint::{
    AuthenticatedCheckpoint, CheckpointFragment, CheckpointSequenceNumber,
};
use crate::object::{Object, ObjectFormatOptions, Owner, OBJECT_START_VERSION};
use crate::storage::{DeleteKind, WriteKind};
use crate::sui_serde::Base64;
use crate::SUI_SYSTEM_STATE_OBJECT_ID;
use base64ct::Encoding;
use byteorder::{BigEndian, ReadBytesExt};
use itertools::Either;
use move_binary_format::access::ModuleAccess;
use move_binary_format::file_format::LocalIndex;
use move_binary_format::CompiledModule;
use move_core_types::language_storage::ModuleId;
use move_core_types::{
    account_address::AccountAddress, identifier::Identifier, language_storage::TypeTag,
    value::MoveStructLayout,
};
use name_variant::NamedVariant;
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use serde_name::{DeserializeNameAdapter, SerializeNameAdapter};
use serde_with::serde_as;
use serde_with::Bytes;
use std::collections::hash_map::DefaultHasher;
use std::fmt::Write;
use std::fmt::{Display, Formatter};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    hash::{Hash, Hasher},
};
use tracing::debug;

#[cfg(test)]
#[path = "unit_tests/messages_tests.rs"]
mod messages_tests;

#[derive(Debug, PartialEq, Eq, Hash, Clone, Serialize, Deserialize)]
pub enum CallArg {
    // contains no structs or objects
    Pure(Vec<u8>),
    // an object
    Object(ObjectArg),
    // a vector of objects
    ObjVec(Vec<ObjectArg>),
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Serialize, Deserialize)]
pub enum ObjectArg {
    // A Move object, either immutable, or owned mutable.
    ImmOrOwnedObject(ObjectRef),
    // A Move object that's shared and mutable.
    SharedObject(ObjectID),
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Serialize, Deserialize)]
pub struct TransferObject {
    pub recipient: SuiAddress,
    pub object_ref: ObjectRef,
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Serialize, Deserialize)]
pub struct MoveCall {
    // Although `package` represents a read-only Move package,
    // we still want to use a reference instead of just object ID.
    // This allows a client to be able to validate the package object
    // used in an order (through the object digest) without having to
    // re-execute the order on a quorum of authorities.
    pub package: ObjectRef,
    pub module: Identifier,
    pub function: Identifier,
    pub type_arguments: Vec<TypeTag>,
    pub arguments: Vec<CallArg>,
}

#[serde_as]
#[derive(Debug, PartialEq, Eq, Hash, Clone, Serialize, Deserialize)]
pub struct MoveModulePublish {
    #[serde_as(as = "Vec<Bytes>")]
    pub modules: Vec<Vec<u8>>,
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Serialize, Deserialize)]
pub struct TransferSui {
    pub recipient: SuiAddress,
    pub amount: Option<u64>,
}

/// Pay each recipient the corresponding amount using the input coins
#[derive(Debug, PartialEq, Eq, Hash, Clone, Serialize, Deserialize)]
pub struct Pay {
    /// The coins to be used for payment
    pub coins: Vec<ObjectRef>,
    /// The addresses that will receive payment
    pub recipients: Vec<SuiAddress>,
    /// The amounts each recipient will receive.
    /// Must be the same length as recipients
    pub amounts: Vec<u64>,
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Serialize, Deserialize)]
pub struct ChangeEpoch {
    /// The next (to become) epoch ID.
    pub epoch: EpochId,
    /// The total amount of gas charged for staroge during the epoch.
    pub storage_charge: u64,
    /// The total amount of gas charged for computation during the epoch.
    pub computation_charge: u64,
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Serialize, Deserialize)]
pub enum SingleTransactionKind {
    /// Initiate an object transfer between addresses
    TransferObject(TransferObject),
    /// Publish a new Move module
    Publish(MoveModulePublish),
    /// Call a function in a published Move module
    Call(MoveCall),
    /// Initiate a SUI coin transfer between addresses
    TransferSui(TransferSui),
    /// Pay multiple recipients using multiple input coins
    Pay(Pay),
    /// A system transaction that will update epoch information on-chain.
    /// It will only ever be executed once in an epoch.
    /// The argument is the next epoch number, which is critical
    /// because it ensures that this transaction has a unique digest.
    /// This will eventually be translated to a Move call during execution.
    /// It also doesn't require/use a gas object.
    /// A validator will not sign a transaction of this kind from outside. It only
    /// signs internally during epoch changes.
    ChangeEpoch(ChangeEpoch),
    // .. more transaction types go here
}

impl SingleTransactionKind {
    pub fn contains_shared_object(&self) -> bool {
        self.shared_input_objects().next().is_some()
    }

    pub fn shared_input_objects(&self) -> impl Iterator<Item = &ObjectID> {
        match &self {
            Self::Call(MoveCall { arguments, .. }) => Either::Left(
                arguments
                    .iter()
                    .filter_map(|arg| match arg {
                        CallArg::Pure(_) | CallArg::Object(ObjectArg::ImmOrOwnedObject(_)) => None,
                        CallArg::Object(ObjectArg::SharedObject(id)) => Some(vec![id]),
                        CallArg::ObjVec(vec) => Some(
                            vec.iter()
                                .filter_map(|obj_arg| {
                                    if let ObjectArg::SharedObject(id) = obj_arg {
                                        Some(id)
                                    } else {
                                        None
                                    }
                                })
                                .collect(),
                        ),
                    })
                    .flatten(),
            ),
            _ => Either::Right(std::iter::empty()),
        }
    }

    pub fn move_call(&self) -> Option<&MoveCall> {
        match &self {
            Self::Call(call @ MoveCall { .. }) => Some(call),
            _ => None,
        }
    }

    /// Return the metadata of each of the input objects for the transaction.
    /// For a Move object, we attach the object reference;
    /// for a Move package, we provide the object id only since they never change on chain.
    /// TODO: use an iterator over references here instead of a Vec to avoid allocations.
    pub fn input_objects(&self) -> SuiResult<Vec<InputObjectKind>> {
        let input_objects = match &self {
            Self::TransferObject(TransferObject { object_ref, .. }) => {
                vec![InputObjectKind::ImmOrOwnedMoveObject(*object_ref)]
            }
            Self::Call(MoveCall {
                arguments, package, ..
            }) => arguments
                .iter()
                .filter_map(|arg| match arg {
                    CallArg::Pure(_) => None,
                    CallArg::Object(ObjectArg::ImmOrOwnedObject(object_ref)) => {
                        Some(vec![InputObjectKind::ImmOrOwnedMoveObject(*object_ref)])
                    }
                    CallArg::Object(ObjectArg::SharedObject(id)) => {
                        Some(vec![InputObjectKind::SharedMoveObject(*id)])
                    }
                    CallArg::ObjVec(vec) => Some(
                        vec.iter()
                            .map(|obj_arg| match obj_arg {
                                ObjectArg::ImmOrOwnedObject(object_ref) => {
                                    InputObjectKind::ImmOrOwnedMoveObject(*object_ref)
                                }
                                ObjectArg::SharedObject(id) => {
                                    InputObjectKind::SharedMoveObject(*id)
                                }
                            })
                            .collect(),
                    ),
                })
                .flatten()
                .chain([InputObjectKind::MovePackage(package.0)])
                .collect(),
            Self::Publish(MoveModulePublish { modules }) => {
                // For module publishing, all the dependent packages are implicit input objects
                // because they must all be on-chain in order for the package to publish.
                // All authorities must have the same view of those dependencies in order
                // to achieve consistent publish results.
                let compiled_modules = modules
                    .iter()
                    .filter_map(|bytes| match CompiledModule::deserialize(bytes) {
                        Ok(m) => Some(m),
                        // We will ignore this error here and simply let latter execution
                        // to discover this error again and fail the transaction.
                        // It's preferable to let transaction fail and charge gas when
                        // malformed package is provided.
                        Err(_) => None,
                    })
                    .collect::<Vec<_>>();
                Transaction::input_objects_in_compiled_modules(&compiled_modules)
            }
            Self::TransferSui(_) => {
                vec![]
            }
            Self::Pay(Pay { coins, .. }) => coins
                .iter()
                .map(|o| InputObjectKind::ImmOrOwnedMoveObject(*o))
                .collect(),
            Self::ChangeEpoch(_) => {
                vec![InputObjectKind::SharedMoveObject(
                    SUI_SYSTEM_STATE_OBJECT_ID,
                )]
            }
        };
        // Ensure that there are no duplicate inputs. This cannot be removed because:
        // In [`AuthorityState::check_locks`], we check that there are no duplicate mutable
        // input objects, which would have made this check here unnecessary. However we
        // do plan to allow shared objects show up more than once in multiple single
        // transactions down the line. Once we have that, we need check here to make sure
        // the same shared object doesn't show up more than once in the same single
        // transaction.
        let mut used = HashSet::new();
        if !input_objects.iter().all(|o| used.insert(o.object_id())) {
            return Err(SuiError::DuplicateObjectRefInput);
        }
        Ok(input_objects)
    }
}

impl Display for SingleTransactionKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut writer = String::new();
        match &self {
            Self::TransferObject(t) => {
                writeln!(writer, "Transaction Kind : Transfer Object")?;
                writeln!(writer, "Recipient : {}", t.recipient)?;
                let (object_id, seq, digest) = t.object_ref;
                writeln!(writer, "Object ID : {}", &object_id)?;
                writeln!(writer, "Sequence Number : {:?}", seq)?;
                writeln!(writer, "Object Digest : {}", encode_bytes_hex(digest.0))?;
            }
            Self::TransferSui(t) => {
                writeln!(writer, "Transaction Kind : Transfer SUI")?;
                writeln!(writer, "Recipient : {}", t.recipient)?;
                if let Some(amount) = t.amount {
                    writeln!(writer, "Amount: {}", amount)?;
                } else {
                    writeln!(writer, "Amount: Full Balance")?;
                }
            }
            Self::Pay(p) => {
                writeln!(writer, "Transaction Kind : Pay")?;
                writeln!(writer, "Coins:")?;
                for (object_id, seq, digest) in &p.coins {
                    writeln!(writer, "Object ID : {}", &object_id)?;
                    writeln!(writer, "Sequence Number : {:?}", seq)?;
                    writeln!(writer, "Object Digest : {}", encode_bytes_hex(digest.0))?;
                }
                writeln!(writer, "Recipients:")?;
                for recipient in &p.recipients {
                    writeln!(writer, "{}", recipient)?;
                }
                writeln!(writer, "Amounts:")?;
                for amount in &p.amounts {
                    writeln!(writer, "{}", amount)?
                }
            }
            Self::Publish(_p) => {
                writeln!(writer, "Transaction Kind : Publish")?;
            }
            Self::Call(c) => {
                writeln!(writer, "Transaction Kind : Call")?;
                writeln!(writer, "Package ID : {}", c.package.0.to_hex_literal())?;
                writeln!(writer, "Module : {}", c.module)?;
                writeln!(writer, "Function : {}", c.function)?;
                writeln!(writer, "Arguments : {:?}", c.arguments)?;
                writeln!(writer, "Type Arguments : {:?}", c.type_arguments)?;
            }
            Self::ChangeEpoch(e) => {
                writeln!(writer, "Transaction Kind: Epoch Change")?;
                writeln!(writer, "New epoch ID: {}", e.epoch)?;
                writeln!(writer, "Storage gas reward: {}", e.storage_charge)?;
                writeln!(writer, "Computation gas reward: {}", e.computation_charge)?;
            }
        }
        write!(f, "{}", writer)
    }
}

// TODO: Make SingleTransactionKind a Box
#[allow(clippy::large_enum_variant)]
#[derive(Debug, PartialEq, Eq, Hash, Clone, Serialize, Deserialize, NamedVariant)]
pub enum TransactionKind {
    /// A single transaction.
    Single(SingleTransactionKind),
    /// A batch of single transactions.
    Batch(Vec<SingleTransactionKind>),
    // .. more transaction types go here
}

impl TransactionKind {
    pub fn single_transactions(&self) -> impl Iterator<Item = &SingleTransactionKind> {
        match self {
            TransactionKind::Single(s) => Either::Left(std::iter::once(s)),
            TransactionKind::Batch(b) => Either::Right(b.iter()),
        }
    }

    pub fn into_single_transactions(self) -> impl Iterator<Item = SingleTransactionKind> {
        match self {
            TransactionKind::Single(s) => Either::Left(std::iter::once(s)),
            TransactionKind::Batch(b) => Either::Right(b.into_iter()),
        }
    }

    pub fn input_objects(&self) -> SuiResult<Vec<InputObjectKind>> {
        let inputs: Vec<_> = self
            .single_transactions()
            .map(|s| s.input_objects())
            .collect::<SuiResult<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect();
        Ok(inputs)
    }

    pub fn shared_input_objects(&self) -> impl Iterator<Item = &ObjectID> {
        match &self {
            TransactionKind::Single(s) => Either::Left(s.shared_input_objects()),
            TransactionKind::Batch(b) => {
                Either::Right(b.iter().flat_map(|kind| kind.shared_input_objects()))
            }
        }
    }

    pub fn batch_size(&self) -> usize {
        match self {
            TransactionKind::Single(_) => 1,
            TransactionKind::Batch(batch) => batch.len(),
        }
    }

    pub fn is_system_tx(&self) -> bool {
        matches!(
            self,
            TransactionKind::Single(SingleTransactionKind::ChangeEpoch(_))
        )
    }

    pub fn is_change_epoch_tx(&self) -> bool {
        matches!(
            self,
            TransactionKind::Single(SingleTransactionKind::ChangeEpoch(_))
        )
    }

    pub fn validity_check(&self) -> SuiResult {
        match self {
            Self::Batch(b) => {
                fp_ensure!(
                    !b.is_empty(),
                    SuiError::InvalidBatchTransaction {
                        error: "Batch Transaction cannot be empty".to_string(),
                    }
                );
                // Check that all transaction kinds can be in a batch.
                let valid = self.single_transactions().all(|s| match s {
                    SingleTransactionKind::Call(_)
                    | SingleTransactionKind::TransferObject(_)
                    | SingleTransactionKind::Pay(_) => true,
                    SingleTransactionKind::TransferSui(_)
                    | SingleTransactionKind::ChangeEpoch(_)
                    | SingleTransactionKind::Publish(_) => false,
                });
                fp_ensure!(
                    valid,
                    SuiError::InvalidBatchTransaction {
                        error: "Batch transaction contains non-batchable transactions. Only Call and TransferObject are allowed".to_string()
                    }
                );
            }
            Self::Single(s) => match s {
                SingleTransactionKind::Pay(_)
                | SingleTransactionKind::Call(_)
                | SingleTransactionKind::Publish(_)
                | SingleTransactionKind::TransferObject(_)
                | SingleTransactionKind::TransferSui(_)
                | SingleTransactionKind::ChangeEpoch(_) => (),
            },
        }
        Ok(())
    }
}

impl Display for TransactionKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut writer = String::new();
        match &self {
            Self::Single(s) => {
                write!(writer, "{}", s)?;
            }
            Self::Batch(b) => {
                writeln!(writer, "Transaction Kind : Batch")?;
                writeln!(writer, "List of transactions in the batch:")?;
                for kind in b {
                    writeln!(writer, "{}", kind)?;
                }
            }
        }
        write!(f, "{}", writer)
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Serialize, Deserialize)]
pub struct TransactionData {
    pub kind: TransactionKind,
    sender: SuiAddress,
    gas_payment: ObjectRef,
    pub gas_price: u64,
    pub gas_budget: u64,
}

impl TransactionData {
    pub fn new(
        kind: TransactionKind,
        sender: SuiAddress,
        gas_payment: ObjectRef,
        gas_budget: u64,
    ) -> Self {
        TransactionData {
            kind,
            sender,
            // TODO: Update local-txn-data-serializer.ts if `gas_price` is changed
            gas_price: 1,
            gas_payment,
            gas_budget,
        }
    }

    pub fn new_with_gas_price(
        kind: TransactionKind,
        sender: SuiAddress,
        gas_payment: ObjectRef,
        gas_budget: u64,
        gas_price: u64,
    ) -> Self {
        TransactionData {
            kind,
            sender,
            gas_price,
            gas_payment,
            gas_budget,
        }
    }

    pub fn new_move_call(
        sender: SuiAddress,
        package: ObjectRef,
        module: Identifier,
        function: Identifier,
        type_arguments: Vec<TypeTag>,
        gas_payment: ObjectRef,
        arguments: Vec<CallArg>,
        gas_budget: u64,
    ) -> Self {
        let kind = TransactionKind::Single(SingleTransactionKind::Call(MoveCall {
            package,
            module,
            function,
            type_arguments,
            arguments,
        }));
        Self::new(kind, sender, gas_payment, gas_budget)
    }

    pub fn new_transfer(
        recipient: SuiAddress,
        object_ref: ObjectRef,
        sender: SuiAddress,
        gas_payment: ObjectRef,
        gas_budget: u64,
    ) -> Self {
        let kind = TransactionKind::Single(SingleTransactionKind::TransferObject(TransferObject {
            recipient,
            object_ref,
        }));
        Self::new(kind, sender, gas_payment, gas_budget)
    }

    pub fn new_transfer_sui(
        recipient: SuiAddress,
        sender: SuiAddress,
        amount: Option<u64>,
        gas_payment: ObjectRef,
        gas_budget: u64,
    ) -> Self {
        let kind = TransactionKind::Single(SingleTransactionKind::TransferSui(TransferSui {
            recipient,
            amount,
        }));
        Self::new(kind, sender, gas_payment, gas_budget)
    }

    pub fn new_pay(
        sender: SuiAddress,
        coins: Vec<ObjectRef>,
        recipients: Vec<SuiAddress>,
        amounts: Vec<u64>,
        gas_payment: ObjectRef,
        gas_budget: u64,
    ) -> Self {
        let kind = TransactionKind::Single(SingleTransactionKind::Pay(Pay {
            coins,
            recipients,
            amounts,
        }));
        Self::new(kind, sender, gas_payment, gas_budget)
    }

    pub fn new_module(
        sender: SuiAddress,
        gas_payment: ObjectRef,
        modules: Vec<Vec<u8>>,
        gas_budget: u64,
    ) -> Self {
        let kind = TransactionKind::Single(SingleTransactionKind::Publish(MoveModulePublish {
            modules,
        }));
        Self::new(kind, sender, gas_payment, gas_budget)
    }

    /// Returns the transaction kind as a &str (variant name, no fields)
    pub fn kind_as_str(&self) -> &'static str {
        self.kind.variant_name()
    }

    pub fn gas(&self) -> ObjectRef {
        self.gas_payment
    }

    pub fn signer(&self) -> SuiAddress {
        self.sender
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut writer = Vec::new();
        self.write(&mut writer);
        writer
    }

    pub fn to_base64(&self) -> String {
        base64ct::Base64::encode_string(&self.to_bytes())
    }

    pub fn gas_payment_object_ref(&self) -> &ObjectRef {
        &self.gas_payment
    }

    pub fn move_calls(&self) -> Vec<&MoveCall> {
        self.kind
            .single_transactions()
            .flat_map(|s| s.move_call())
            .collect()
    }

    pub fn input_objects(&self) -> SuiResult<Vec<InputObjectKind>> {
        let mut inputs = self.kind.input_objects()?;

        if !self.kind.is_system_tx() {
            inputs.push(InputObjectKind::ImmOrOwnedMoveObject(
                *self.gas_payment_object_ref(),
            ));
        }
        Ok(inputs)
    }
}

/// A transaction signed by a client, optionally signed by an authority (depending on `S`).
/// `S` indicates the authority signing state. It can be either empty or signed.
/// We make the authority signature templated so that `TransactionEnvelope<S>` can be used
/// universally in the transactions storage in `SuiDataStore`, shared by both authorities
/// and non-authorities: authorities store signed transactions, while non-authorities
/// store unsigned transactions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(remote = "TransactionEnvelope")]
pub struct TransactionEnvelope<S> {
    // This is a cache of an otherwise expensive to compute value.
    // DO NOT serialize or deserialize from the network or disk.
    #[serde(skip)]
    transaction_digest: OnceCell<TransactionDigest>,
    // Deserialization sets this to "false"
    // TODO: is_verified is only set to true in some callsites after verification.
    // Hence it's not optimal.
    #[serde(skip)]
    pub is_verified: bool,

    // The packet of data that authorities will sign on. Stores the tx data and the sender signature.
    pub signed_data: SenderSignedData,

    /// authority signature information, if available, is signed by an authority, applied on `tx_signature` || `data`.
    pub auth_sign_info: S,
    // Note: If any new field is added here, make sure the Hash and PartialEq
    // implementation are adjusted to include that new field (unless the new field
    // does not participate in the hash and comparison).
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct SenderSignedData {
    pub data: TransactionData,
    /// tx_signature is signed by the transaction sender, applied on `data`.
    pub tx_signature: Signature,
}

impl<S> TransactionEnvelope<S> {
    #[allow(dead_code)]
    fn add_sender_sig_to_verification_obligation(
        &self,
        obligation: &mut VerificationObligation,
        idx: usize,
    ) -> SuiResult<()> {
        // We use this flag to see if someone has checked this before
        // and therefore we can skip the check. Note that the flag has
        // to be set to true manually, and is not set by calling this
        // "check" function.
        if self.is_verified || self.signed_data.data.kind.is_system_tx() {
            return Ok(());
        }

        self.signed_data
            .tx_signature
            .add_to_verification_obligation_or_verify(self.signed_data.data.sender, obligation, idx)
    }

    pub fn verify_sender_signature(&self) -> SuiResult<()> {
        if self.is_verified || self.signed_data.data.kind.is_system_tx() {
            return Ok(());
        }
        self.signed_data
            .tx_signature
            .verify(&self.signed_data.data, self.signed_data.data.sender)
    }

    pub fn sender_address(&self) -> SuiAddress {
        self.signed_data.data.sender
    }

    pub fn gas_payment_object_ref(&self) -> &ObjectRef {
        self.signed_data.data.gas_payment_object_ref()
    }

    pub fn contains_shared_object(&self) -> bool {
        self.shared_input_objects().next().is_some()
    }

    pub fn shared_input_objects(&self) -> impl Iterator<Item = &ObjectID> {
        self.signed_data.data.kind.shared_input_objects()
    }

    /// Get the transaction digest and write it to the cache
    pub fn digest(&self) -> &TransactionDigest {
        self.transaction_digest
            .get_or_init(|| TransactionDigest::new(sha3_hash(&self.signed_data)))
    }

    pub fn input_objects_in_compiled_modules(
        compiled_modules: &[CompiledModule],
    ) -> Vec<InputObjectKind> {
        let to_be_published: BTreeSet<_> = compiled_modules.iter().map(|m| m.self_id()).collect();
        let mut dependent_packages = BTreeSet::new();
        for module in compiled_modules {
            for handle in &module.module_handles {
                if !to_be_published.contains(&module.module_id_for_handle(handle)) {
                    let address = ObjectID::from(*module.address_identifier_at(handle.address));
                    dependent_packages.insert(address);
                }
            }
        }

        // We don't care about the digest of the dependent packages.
        // They are all read-only on-chain and their digest never changes.
        dependent_packages
            .into_iter()
            .map(InputObjectKind::MovePackage)
            .collect::<Vec<_>>()
    }

    pub fn is_system_tx(&self) -> bool {
        self.signed_data.data.kind.is_system_tx()
    }
}

// In combination with #[serde(remote = "TransactionEnvelope")].
// Generic types instantiated multiple times in the same tracing session requires a work around.
// https://novifinancial.github.io/serde-reflection/serde_reflection/index.html#features-and-limitations
impl<'de, T> Deserialize<'de> for TransactionEnvelope<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::de::Deserializer<'de>,
    {
        TransactionEnvelope::deserialize(DeserializeNameAdapter::new(
            deserializer,
            // TODO: This generates a very long name that includes the namespace and modules.
            // Ideally we just want TransactionEnvelope<T> with T substituted as the name.
            // https://github.com/MystenLabs/sui/issues/1119
            std::any::type_name::<TransactionEnvelope<T>>(),
        ))
    }
}

impl<T> Serialize for TransactionEnvelope<T>
where
    T: Serialize,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::ser::Serializer,
    {
        TransactionEnvelope::serialize(
            self,
            SerializeNameAdapter::new(serializer, std::any::type_name::<TransactionEnvelope<T>>()),
        )
    }
}

// TODO: this should maybe be called ClientSignedTransaction + SignedTransaction -> AuthoritySignedTransaction.
/// A transaction that is signed by a sender but not yet by an authority.
pub type Transaction = TransactionEnvelope<EmptySignInfo>;

impl Transaction {
    #[cfg(test)]
    pub fn from_data(data: TransactionData, signer: &dyn signature::Signer<Signature>) -> Self {
        let signature = Signature::new(&data, signer);
        Self::new(data, signature)
    }

    pub fn new(data: TransactionData, signature: Signature) -> Self {
        Self {
            transaction_digest: OnceCell::new(),
            is_verified: false,
            signed_data: SenderSignedData {
                data,
                tx_signature: signature,
            },
            auth_sign_info: EmptySignInfo {},
        }
    }

    pub fn verify(&self) -> Result<(), SuiError> {
        self.verify_sender_signature()
    }

    pub fn to_network_data_for_execution(&self) -> (Base64, SignatureScheme, Base64, Base64) {
        (
            Base64::from_bytes(&self.signed_data.data.to_bytes()),
            self.signed_data.tx_signature.scheme(),
            Base64::from_bytes(self.signed_data.tx_signature.signature_bytes()),
            Base64::from_bytes(self.signed_data.tx_signature.public_key_bytes()),
        )
    }
}

impl Hash for Transaction {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.signed_data.hash(state);
    }
}

impl PartialEq for Transaction {
    fn eq(&self, other: &Self) -> bool {
        self.signed_data == other.signed_data
    }
}
impl Eq for Transaction {}

/// A transaction that is signed by a sender and also by an authority.
pub type SignedTransaction = TransactionEnvelope<AuthoritySignInfo>;

impl SignedTransaction {
    /// Use signing key to create a signed object.
    pub fn new(
        epoch: EpochId,
        transaction: Transaction,
        authority: AuthorityName,
        secret: &dyn signature::Signer<AuthoritySignature>,
    ) -> Self {
        let signature = AuthoritySignature::new(&transaction.signed_data, secret);
        Self {
            transaction_digest: OnceCell::new(),
            is_verified: transaction.is_verified,
            signed_data: transaction.signed_data,
            auth_sign_info: AuthoritySignInfo {
                epoch,
                authority,
                signature,
            },
        }
    }

    pub fn new_change_epoch(
        next_epoch: EpochId,
        storage_charge: u64,
        computation_charge: u64,
        authority: AuthorityName,
        secret: &dyn signature::Signer<AuthoritySignature>,
    ) -> Self {
        let kind = TransactionKind::Single(SingleTransactionKind::ChangeEpoch(ChangeEpoch {
            epoch: next_epoch,
            storage_charge,
            computation_charge,
        }));
        // For the ChangeEpoch transaction, we do not care about the sender and the gas.
        let data = TransactionData::new(
            kind,
            SuiAddress::default(),
            (ObjectID::ZERO, SequenceNumber::default(), ObjectDigest::MIN),
            0,
        );
        let signed_data = SenderSignedData {
            data,
            // Arbitrary keypair
            tx_signature: Ed25519SuiSignature::from_bytes(&[0; Ed25519SuiSignature::LENGTH])
                .unwrap()
                .into(),
        };
        let signature = AuthoritySignature::new(&signed_data, secret);
        Self {
            transaction_digest: OnceCell::new(),
            is_verified: false,
            signed_data,
            auth_sign_info: AuthoritySignInfo {
                epoch: next_epoch,
                authority,
                signature,
            },
        }
    }

    /// Verify the signature and return the non-zero voting right of the authority.
    pub fn verify(&self, committee: &Committee) -> SuiResult {
        self.verify_sender_signature()?;

        let mut obligation = VerificationObligation::default();
        let idx = obligation.add_message(&self.signed_data);
        self.auth_sign_info
            .add_to_verification_obligation(committee, &mut obligation, idx)?;

        obligation.verify_all()?;
        Ok(())
    }

    // Turn a SignedTransaction into a Transaction. This is needed when we are
    // forming a CertifiedTransaction, where each transaction's authority signature
    // is taking out to form an aggregated signature.
    pub fn to_transaction(self) -> Transaction {
        Transaction::new(self.signed_data.data, self.signed_data.tx_signature)
    }
}

impl Hash for SignedTransaction {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.signed_data.hash(state);
        self.auth_sign_info.hash(state);
    }
}

impl PartialEq for SignedTransaction {
    fn eq(&self, other: &Self) -> bool {
        // We do not compare the tx_signature, because there can be multiple
        // valid signatures for the same data and signer.
        self.signed_data == other.signed_data && self.auth_sign_info == other.auth_sign_info
    }
}

pub type CertifiedTransaction = TransactionEnvelope<AuthorityStrongQuorumSignInfo>;
pub type TxCertAndSignedEffects = (CertifiedTransaction, SignedTransactionEffects);

#[derive(Debug, PartialEq, Eq, Hash, Clone, Serialize, Deserialize)]
pub struct AccountInfoRequest {
    pub account: SuiAddress,
}

/// An information Request for batches, and their associated transactions
///
/// This reads historic data and sends the batch and transactions in the
/// database starting at the batch that includes `start`,
/// and then listens to new transactions until a batch equal or
/// is over the batch end marker.
#[derive(Debug, PartialEq, Eq, Hash, Clone, Serialize, Deserialize)]
pub struct BatchInfoRequest {
    // The sequence number at which to start the sequence to return, or None for the latest.
    pub start: Option<TxSequenceNumber>,
    // The total number of items to receive. Could receive a bit more or a bit less.
    pub length: u64,
}

#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct BatchInfoResponseItem(pub UpdateItem);

/// Subscribe to notifications when new checkpoint certificates are available.
///
/// Note that there is no start field necessary, because checkpoint sequence numbers are
/// contiguous. Therefore the client is always immediately sent the highest available checkpoint
/// number, from which they can deduce if they are missing any checkpoints.
#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointStreamRequest {
    // No request fields are currently necessary, but tonic errors when the request struct has size
    // 0.
    _ignored: u64,
}

impl CheckpointStreamRequest {
    pub fn new() -> Self {
        Default::default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointStreamResponseItem {
    /// The first available checkpoint sequence on this validator. Currently this is always 0.
    /// When snapshots are implemented, this may change to become the first checkpoint after the
    /// most recent snapshot.
    pub first_available_sequence: CheckpointSequenceNumber,
    pub checkpoint: AuthenticatedCheckpoint,
}

impl From<SuiAddress> for AccountInfoRequest {
    fn from(account: SuiAddress) -> Self {
        AccountInfoRequest { account }
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Serialize, Deserialize)]
pub enum ObjectInfoRequestKind {
    /// Request the latest object state, if a format option is provided,
    /// return the layout of the object in the given format.
    LatestObjectInfo(Option<ObjectFormatOptions>),
    /// Request the object state at a specific version
    PastObjectInfo(SequenceNumber),
    /// Similar to PastObjectInfo, except that it will also return the object content.
    /// This is used only for debugging purpose and will not work in the long run when
    /// we stop storing all historic versions of every object.
    /// No production code should depend on this kind.
    PastObjectInfoDebug(SequenceNumber, Option<ObjectFormatOptions>),
}

/// A request for information about an object and optionally its
/// parent certificate at a specific version.
#[derive(Debug, PartialEq, Eq, Hash, Clone, Serialize, Deserialize)]
pub struct ObjectInfoRequest {
    /// The id of the object to retrieve, at the latest version.
    pub object_id: ObjectID,
    /// The type of request, either latest object info or the past.
    pub request_kind: ObjectInfoRequestKind,
}

impl ObjectInfoRequest {
    pub fn past_object_info_request(object_id: ObjectID, version: SequenceNumber) -> Self {
        ObjectInfoRequest {
            object_id,
            request_kind: ObjectInfoRequestKind::PastObjectInfo(version),
        }
    }

    pub fn latest_object_info_request(
        object_id: ObjectID,
        layout: Option<ObjectFormatOptions>,
    ) -> Self {
        ObjectInfoRequest {
            object_id,
            request_kind: ObjectInfoRequestKind::LatestObjectInfo(layout),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Serialize, Deserialize)]
pub struct AccountInfoResponse {
    pub object_ids: Vec<ObjectRef>,
    pub owner: SuiAddress,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectResponse {
    /// Value of the requested object in this authority
    pub object: Object,
    /// Transaction the object is locked on in this authority.
    /// None if the object is not currently locked by this authority.
    pub lock: Option<SignedTransaction>,
    /// Schema of the Move value inside this object.
    /// None if the object is a Move package, or the request did not ask for the layout
    pub layout: Option<MoveStructLayout>,
}

/// This message provides information about the latest object and its lock
/// as well as the parent certificate of the object at a specific version.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectInfoResponse {
    /// The certificate that created or mutated the object at a given version.
    /// If no parent certificate was requested the latest certificate concerning
    /// this object is sent. If the parent was requested and not found a error
    /// (ParentNotfound or CertificateNotfound) will be returned.
    pub parent_certificate: Option<CertifiedTransaction>,
    /// The full reference created by the above certificate
    pub requested_object_reference: Option<ObjectRef>,

    /// The object and its current lock, returned only if we are requesting
    /// the latest state of an object.
    /// If the object does not exist this is also None.
    pub object_and_lock: Option<ObjectResponse>,
}

impl ObjectInfoResponse {
    pub fn object(&self) -> Option<&Object> {
        match &self.object_and_lock {
            Some(ObjectResponse { object, .. }) => Some(object),
            _ => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Serialize, Deserialize)]
pub struct TransactionInfoRequest {
    pub transaction_digest: TransactionDigest,
}

impl From<TransactionDigest> for TransactionInfoRequest {
    fn from(transaction_digest: TransactionDigest) -> Self {
        TransactionInfoRequest { transaction_digest }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransactionInfoResponse {
    // The signed transaction response to handle_transaction
    pub signed_transaction: Option<SignedTransaction>,
    // The certificate in case one is available
    pub certified_transaction: Option<CertifiedTransaction>,
    // The effects resulting from a successful execution should
    // contain ObjectRef created, mutated, deleted and events.
    pub signed_effects: Option<SignedTransactionEffects>,
}

#[derive(Eq, PartialEq, Clone, Debug, Serialize, Deserialize)]
pub enum CallResult {
    Bool(bool),
    U8(u8),
    U64(u64),
    U128(u128),
    Address(AccountAddress),
    // these are not ideal but there is no other way to deserialize
    // vectors encoded in BCS (you need a full type before this can be
    // done)
    BoolVec(Vec<bool>),
    U8Vec(Vec<u8>),
    U64Vec(Vec<u64>),
    U128Vec(Vec<u128>),
    AddrVec(Vec<AccountAddress>),
    BoolVecVec(Vec<bool>),
    U8VecVec(Vec<Vec<u8>>),
    U64VecVec(Vec<Vec<u64>>),
    U128VecVec(Vec<Vec<u128>>),
    AddrVecVec(Vec<Vec<AccountAddress>>),
}

#[derive(Eq, PartialEq, Clone, Debug, Serialize, Deserialize)]
pub enum ExecutionStatus {
    Success,
    // Gas used in the failed case, and the error.
    Failure { error: ExecutionFailureStatus },
}

#[derive(Eq, PartialEq, Clone, Debug, Serialize, Deserialize)]
pub enum ExecutionFailureStatus {
    //
    // General transaction errors
    //
    InsufficientGas,
    InvalidGasObject,
    InvalidTransactionUpdate,
    ModuleNotFound,
    FunctionNotFound,
    InvariantViolation,

    //
    // Transfer errors
    //
    InvalidTransferObject,
    InvalidTransferSui,
    InvalidTransferSuiInsufficientBalance,
    InvalidCoinObject,

    //
    // Pay errors
    //
    /// Supplied 0 input coins
    EmptyInputCoins,
    /// Supplied an empty list of recipient addresses for the payment
    EmptyRecipients,
    /// Supplied a different number of recipient addresses and recipient amounts
    RecipientsAmountsArityMismatch,
    /// Not enough funds to perform the requested payment
    InsufficientBalance,

    //
    // MoveCall errors
    //
    NonEntryFunctionInvoked,
    EntryTypeArityMismatch,
    EntryArgumentError(EntryArgumentError),
    CircularObjectOwnership(CircularObjectOwnership),
    MissingObjectOwner(MissingObjectOwner),
    InvalidSharedChildUse(InvalidSharedChildUse),
    InvalidSharedByValue(InvalidSharedByValue),
    TooManyChildObjects {
        object: ObjectID,
    },
    InvalidParentDeletion {
        parent: ObjectID,
        kind: Option<DeleteKind>,
    },
    InvalidParentFreezing {
        parent: ObjectID,
    },

    //
    // MovePublish errors
    //
    PublishErrorEmptyPackage,
    PublishErrorNonZeroAddress,
    PublishErrorDuplicateModule,
    SuiMoveVerificationError,

    //
    // Errors from the Move VM
    //
    // TODO module id + func def + offset?
    MovePrimitiveRuntimeError,
    /// Indicates and `abort` from inside Move code. Contains the location of the abort and the
    /// abort code
    MoveAbort(ModuleId, u64), // TODO func def + offset?
    VMVerificationOrDeserializationError,
    VMInvariantViolation,
}

#[derive(Eq, PartialEq, Clone, Copy, Debug, Serialize, Deserialize, Hash)]
pub struct EntryArgumentError {
    pub argument_idx: LocalIndex,
    pub kind: EntryArgumentErrorKind,
}

#[derive(Eq, PartialEq, Clone, Copy, Debug, Serialize, Deserialize, Hash)]
pub enum EntryArgumentErrorKind {
    TypeMismatch,
    InvalidObjectByValue,
    InvalidObjectByMuteRef,
    ObjectKindMismatch,
    UnsupportedPureArg,
    ArityMismatch,
}

#[derive(Eq, PartialEq, Clone, Copy, Debug, Serialize, Deserialize, Hash)]
pub struct CircularObjectOwnership {
    pub object: ObjectID,
}

#[derive(Eq, PartialEq, Clone, Copy, Debug, Serialize, Deserialize, Hash)]
pub struct MissingObjectOwner {
    pub child: ObjectID,
    pub parent: SuiAddress,
}

#[derive(Eq, PartialEq, Clone, Copy, Debug, Serialize, Deserialize, Hash)]
pub struct InvalidSharedChildUse {
    pub child: ObjectID,
    pub ancestor: ObjectID,
}

#[derive(Eq, PartialEq, Clone, Copy, Debug, Serialize, Deserialize, Hash)]
pub struct InvalidSharedByValue {
    pub object: ObjectID,
}

impl ExecutionFailureStatus {
    pub fn entry_argument_error(argument_idx: LocalIndex, kind: EntryArgumentErrorKind) -> Self {
        EntryArgumentError { argument_idx, kind }.into()
    }

    pub fn circular_object_ownership(object: ObjectID) -> Self {
        CircularObjectOwnership { object }.into()
    }

    pub fn missing_object_owner(child: ObjectID, parent: SuiAddress) -> Self {
        MissingObjectOwner { child, parent }.into()
    }

    pub fn invalid_shared_child_use(child: ObjectID, ancestor: ObjectID) -> Self {
        InvalidSharedChildUse { child, ancestor }.into()
    }

    pub fn invalid_shared_by_value(object: ObjectID) -> Self {
        InvalidSharedByValue { object }.into()
    }
}

impl Display for ExecutionFailureStatus {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecutionFailureStatus::EmptyInputCoins => {
                write!(f, "Expected a non-empty list of input Coin objects")
            }
            ExecutionFailureStatus::EmptyRecipients => {
                write!(f, "Expected a non-empty list of recipient addresses")
            }
            ExecutionFailureStatus::InsufficientBalance => write!(
                f,
                "Value of input coins is insufficient to cover outgoing amounts"
            ),
            ExecutionFailureStatus::InsufficientGas => write!(f, "Insufficient Gas."),
            ExecutionFailureStatus::InvalidGasObject => {
                write!(
                    f,
                    "Invalid Gas Object. Possibly not address-owned or possibly not a SUI coin."
                )
            }
            ExecutionFailureStatus::InvalidTransactionUpdate => {
                write!(f, "Invalid Transaction Update.")
            }
            ExecutionFailureStatus::ModuleNotFound => write!(f, "Module Not Found."),
            ExecutionFailureStatus::FunctionNotFound => write!(f, "Function Not Found."),
            ExecutionFailureStatus::InvariantViolation => write!(f, "INVARIANT VIOLATION."),
            ExecutionFailureStatus::InvalidTransferObject => write!(
                f,
                "Invalid Transfer Object Transaction. \
                Possibly not address-owned or possibly does not have public transfer."
            ),
            ExecutionFailureStatus::InvalidCoinObject => {
                write!(f, "Invalid coin::Coin object bytes.")
            }
            ExecutionFailureStatus::InvalidTransferSui => write!(
                f,
                "Invalid Transfer SUI. \
                Possibly not address-owned or possibly not a SUI coin."
            ),
            ExecutionFailureStatus::InvalidTransferSuiInsufficientBalance => {
                write!(f, "Invalid Transfer SUI, Insufficient Balance.")
            }
            ExecutionFailureStatus::NonEntryFunctionInvoked => write!(
                f,
                "Non Entry Function Invoked. Move Call must start with an entry function"
            ),
            ExecutionFailureStatus::EntryTypeArityMismatch => write!(
                f,
                "Number of type arguments does not match the expected value",
            ),
            ExecutionFailureStatus::EntryArgumentError(data) => {
                write!(f, "Entry Argument Type Error. {data}")
            }
            ExecutionFailureStatus::CircularObjectOwnership(data) => {
                write!(f, "Circular  Object Ownership. {data}")
            }
            ExecutionFailureStatus::MissingObjectOwner(data) => {
                write!(f, "Missing Object Owner. {data}")
            }
            ExecutionFailureStatus::InvalidSharedChildUse(data) => {
                write!(f, "Invalid Shared Child Object Usage. {data}.")
            }
            ExecutionFailureStatus::InvalidSharedByValue(data) => {
                write!(f, "Invalid Shared Object By-Value Usage. {data}.")
            }
            ExecutionFailureStatus::RecipientsAmountsArityMismatch => write!(
                f,
                "Expected recipient and amounts lists to be the same length"
            ),
            ExecutionFailureStatus::TooManyChildObjects { object } => {
                write!(
                    f,
                    "Object {object} has too many child objects. \
                    The number of child objects cannot exceed 2^32 - 1."
                )
            }
            ExecutionFailureStatus::InvalidParentDeletion { parent, kind } => {
                let method = match kind {
                    Some(DeleteKind::Normal) => "deleted",
                    Some(DeleteKind::UnwrapThenDelete) => "unwrapped then deleted",
                    Some(DeleteKind::Wrap) => "wrapped in another object",
                    None => "created and destroyed",
                };
                write!(
                    f,
                    "Invalid Deletion of Parent Object with Children. Parent object {parent} was \
                    {method} before its children were deleted or transferred."
                )
            }
            ExecutionFailureStatus::InvalidParentFreezing { parent } => {
                write!(
                    f,
                    "Invalid Freezing of Parent Object with Children. Parent object {parent} was \
                    made immutable before its children were deleted or transferred."
                )
            }
            ExecutionFailureStatus::PublishErrorEmptyPackage => write!(
                f,
                "Publish Error, Empty Package. A package must have at least one module."
            ),
            ExecutionFailureStatus::PublishErrorNonZeroAddress => write!(
                f,
                "Publish Error, Non-zero Address. \
                The modules in the package must have their address set to zero."
            ),
            ExecutionFailureStatus::PublishErrorDuplicateModule => write!(
                f,
                "Publish Error, Duplicate Module. More than one module with a given name."
            ),
            ExecutionFailureStatus::SuiMoveVerificationError => write!(
                f,
                "Sui Move Bytecode Verification Error. \
                Please run the Sui Move Verifier for more information."
            ),
            ExecutionFailureStatus::MovePrimitiveRuntimeError => write!(
                f,
                "Move Primitive Runtime Error. \
                Arithmetic error, stack overflow, max value depth, etc."
            ),
            ExecutionFailureStatus::MoveAbort(m, c) => {
                write!(f, "Move Runtime Abort. Module: {}, Status Code: {}", m, c)
            }
            ExecutionFailureStatus::VMVerificationOrDeserializationError => write!(
                f,
                "Move Bytecode Verification Error. \
                Please run the Bytecode Verifier for more information."
            ),
            ExecutionFailureStatus::VMInvariantViolation => {
                write!(f, "MOVE VM INVARIANT VIOLATION.")
            }
        }
    }
}

impl Display for EntryArgumentError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let EntryArgumentError { argument_idx, kind } = self;
        write!(f, "Error for argument at index {argument_idx}: {kind}",)
    }
}

impl Display for EntryArgumentErrorKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            EntryArgumentErrorKind::TypeMismatch => write!(f, "Type mismatch."),
            EntryArgumentErrorKind::InvalidObjectByValue => {
                write!(f, "Immutable and shared objects cannot be passed by-value.")
            }
            EntryArgumentErrorKind::InvalidObjectByMuteRef => {
                write!(
                    f,
                    "Immutable objects cannot be passed by mutable reference, &mut."
                )
            }
            EntryArgumentErrorKind::ObjectKindMismatch => {
                write!(f, "Mismtach with object argument kind and its actual kind.")
            }
            EntryArgumentErrorKind::UnsupportedPureArg => write!(
                f,
                "Unsupported non-object argument; if it is an object, it must be \
                populated by an object ID."
            ),
            EntryArgumentErrorKind::ArityMismatch => {
                write!(
                    f,
                    "Mismatch between the number of actual versus expected argument."
                )
            }
        }
    }
}

impl Display for CircularObjectOwnership {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let CircularObjectOwnership { object } = self;
        write!(f, "Circular object ownership, including object {object}.")
    }
}

impl Display for MissingObjectOwner {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let MissingObjectOwner { child, parent } = self;
        write!(
            f,
            "Missing object owner, the parent object {parent} for child object {child}.",
        )
    }
}

impl Display for InvalidSharedChildUse {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let InvalidSharedChildUse { child, ancestor } = self;
        write!(
            f,
            "When a child object (either direct or indirect) of a shared object is passed by-value \
            to an entry function, either the child object's type or the shared object's type must \
            be defined in the same module as the called function. This is violated by object \
            {child}, whose ancestor {ancestor} is a shared object, and neither are defined in \
            this module.",
        )
    }
}

impl Display for InvalidSharedByValue {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let InvalidSharedByValue { object } = self;
        write!(
            f,
        "When a shared object is passed as an owned Move value in an entry function, either the \
        the shared object's type must be defined in the same module as the called function. The \
        shared object {object} is not defined in this module",
        )
    }
}

impl std::error::Error for ExecutionFailureStatus {}

impl ExecutionStatus {
    pub fn new_failure(error: ExecutionFailureStatus) -> ExecutionStatus {
        ExecutionStatus::Failure { error }
    }

    pub fn is_ok(&self) -> bool {
        matches!(self, ExecutionStatus::Success { .. })
    }

    pub fn is_err(&self) -> bool {
        matches!(self, ExecutionStatus::Failure { .. })
    }

    pub fn unwrap(self) {
        match self {
            ExecutionStatus::Success => {}
            ExecutionStatus::Failure { .. } => {
                panic!("Unable to unwrap() on {:?}", self);
            }
        }
    }

    pub fn unwrap_err(self) -> ExecutionFailureStatus {
        match self {
            ExecutionStatus::Success { .. } => {
                panic!("Unable to unwrap() on {:?}", self);
            }
            ExecutionStatus::Failure { error } => error,
        }
    }
}

impl From<EntryArgumentError> for ExecutionFailureStatus {
    fn from(error: EntryArgumentError) -> Self {
        Self::EntryArgumentError(error)
    }
}

impl From<CircularObjectOwnership> for ExecutionFailureStatus {
    fn from(error: CircularObjectOwnership) -> Self {
        Self::CircularObjectOwnership(error)
    }
}

impl From<MissingObjectOwner> for ExecutionFailureStatus {
    fn from(error: MissingObjectOwner) -> Self {
        Self::MissingObjectOwner(error)
    }
}

impl From<InvalidSharedChildUse> for ExecutionFailureStatus {
    fn from(error: InvalidSharedChildUse) -> Self {
        Self::InvalidSharedChildUse(error)
    }
}

impl From<InvalidSharedByValue> for ExecutionFailureStatus {
    fn from(error: InvalidSharedByValue) -> Self {
        Self::InvalidSharedByValue(error)
    }
}

/// The response from processing a transaction or a certified transaction
#[derive(Eq, PartialEq, Clone, Debug, Serialize, Deserialize)]
pub struct TransactionEffects {
    // The status of the execution
    pub status: ExecutionStatus,
    pub gas_used: GasCostSummary,
    // The object references of the shared objects used in this transaction. Empty if no shared objects were used.
    pub shared_objects: Vec<ObjectRef>,
    // The transaction digest
    pub transaction_digest: TransactionDigest,
    // ObjectRef and owner of new objects created.
    pub created: Vec<(ObjectRef, Owner)>,
    // ObjectRef and owner of mutated objects, including gas object.
    pub mutated: Vec<(ObjectRef, Owner)>,
    // ObjectRef and owner of objects that are unwrapped in this transaction.
    // Unwrapped objects are objects that were wrapped into other objects in the past,
    // and just got extracted out.
    pub unwrapped: Vec<(ObjectRef, Owner)>,
    // Object Refs of objects now deleted (the old refs).
    pub deleted: Vec<ObjectRef>,
    // Object refs of objects now wrapped in other objects.
    pub wrapped: Vec<ObjectRef>,
    // The updated gas object reference. Have a dedicated field for convenient access.
    // It's also included in mutated.
    pub gas_object: (ObjectRef, Owner),
    /// The events emitted during execution. Note that only successful transactions emit events
    pub events: Vec<Event>,
    /// The set of transaction digests this transaction depends on.
    pub dependencies: Vec<TransactionDigest>,
}

impl TransactionEffects {
    /// Return an iterator that iterates through all mutated objects, including mutated,
    /// created and unwrapped objects. In other words, all objects that still exist
    /// in the object state after this transaction.
    /// It doesn't include deleted/wrapped objects.
    pub fn all_mutated(&self) -> impl Iterator<Item = (&ObjectRef, &Owner, WriteKind)> + Clone {
        self.mutated
            .iter()
            .map(|(r, o)| (r, o, WriteKind::Mutate))
            .chain(self.created.iter().map(|(r, o)| (r, o, WriteKind::Create)))
            .chain(
                self.unwrapped
                    .iter()
                    .map(|(r, o)| (r, o, WriteKind::Unwrap)),
            )
    }

    /// Return an iterator of mutated objects, but excluding the gas object.
    pub fn mutated_excluding_gas(&self) -> impl Iterator<Item = &(ObjectRef, Owner)> {
        self.mutated.iter().filter(|o| *o != &self.gas_object)
    }

    pub fn gas_cost_summary(&self) -> &GasCostSummary {
        &self.gas_used
    }

    pub fn is_object_mutated_here(&self, obj_ref: ObjectRef) -> bool {
        // The mutated or created case
        if self.all_mutated().any(|(oref, _, _)| *oref == obj_ref) {
            return true;
        }

        // The deleted case
        if obj_ref.2 == ObjectDigest::OBJECT_DIGEST_DELETED
            && self
                .deleted
                .iter()
                .any(|(id, seq, _)| *id == obj_ref.0 && seq.increment() == obj_ref.1)
        {
            return true;
        }

        // The wrapped case
        if obj_ref.2 == ObjectDigest::OBJECT_DIGEST_WRAPPED
            && self
                .wrapped
                .iter()
                .any(|(id, seq, _)| *id == obj_ref.0 && seq.increment() == obj_ref.1)
        {
            return true;
        }
        false
    }

    pub fn to_sign_effects(
        self,
        epoch: EpochId,
        authority_name: &AuthorityName,
        secret: &dyn signature::Signer<AuthoritySignature>,
    ) -> SignedTransactionEffects {
        let signature = AuthoritySignature::new(&self, secret);
        let transaction_effects_digest = OnceCell::from(self.digest());

        SignedTransactionEffects {
            transaction_effects_digest,
            effects: self,
            auth_signature: AuthoritySignInfo {
                epoch,
                authority: *authority_name,
                signature,
            },
        }
    }

    pub fn digest(&self) -> TransactionEffectsDigest {
        TransactionEffectsDigest(sha3_hash(self))
    }
}

impl Display for TransactionEffects {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut writer = String::new();
        writeln!(writer, "Status : {:?}", self.status)?;
        if !self.created.is_empty() {
            writeln!(writer, "Created Objects:")?;
            for ((id, _, _), owner) in &self.created {
                writeln!(writer, "  - ID: {} , Owner: {}", id, owner)?;
            }
        }
        if !self.mutated.is_empty() {
            writeln!(writer, "Mutated Objects:")?;
            for ((id, _, _), owner) in &self.mutated {
                writeln!(writer, "  - ID: {} , Owner: {}", id, owner)?;
            }
        }
        if !self.deleted.is_empty() {
            writeln!(writer, "Deleted Objects:")?;
            for (id, _, _) in &self.deleted {
                writeln!(writer, "  - ID: {}", id)?;
            }
        }
        if !self.wrapped.is_empty() {
            writeln!(writer, "Wrapped Objects:")?;
            for (id, _, _) in &self.wrapped {
                writeln!(writer, "  - ID: {}", id)?;
            }
        }
        if !self.unwrapped.is_empty() {
            writeln!(writer, "Unwrapped Objects:")?;
            for ((id, _, _), owner) in &self.unwrapped {
                writeln!(writer, "  - ID: {} , Owner: {}", id, owner)?;
            }
        }
        write!(f, "{}", writer)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransactionEffectsEnvelope<S> {
    // This is a cache of an otherwise expensive to compute value.
    // DO NOT serialize or deserialize from the network or disk.
    #[serde(skip)]
    transaction_effects_digest: OnceCell<TransactionEffectsDigest>,

    pub effects: TransactionEffects,
    pub auth_signature: S,
}

impl<S> TransactionEffectsEnvelope<S> {
    pub fn digest(&self) -> &TransactionEffectsDigest {
        self.transaction_effects_digest
            .get_or_init(|| self.effects.digest())
    }
}

pub type UnsignedTransactionEffects = TransactionEffectsEnvelope<EmptySignInfo>;
pub type SignedTransactionEffects = TransactionEffectsEnvelope<AuthoritySignInfo>;

impl SignedTransactionEffects {
    pub fn verify(&self, committee: &Committee) -> SuiResult {
        self.auth_signature.verify(&self.effects, committee)
    }
}

impl PartialEq for SignedTransactionEffects {
    fn eq(&self, other: &Self) -> bool {
        self.effects == other.effects && self.auth_signature == other.auth_signature
    }
}

pub type CertifiedTransactionEffects = TransactionEffectsEnvelope<AuthorityStrongQuorumSignInfo>;

impl CertifiedTransactionEffects {
    pub fn new(
        effects: TransactionEffects,
        signatures: Vec<(AuthorityName, AuthoritySignature)>,
        committee: &Committee,
    ) -> SuiResult<Self> {
        Ok(Self {
            transaction_effects_digest: OnceCell::from(effects.digest()),
            effects,
            auth_signature: AuthorityStrongQuorumSignInfo::new_with_signatures(
                signatures, committee,
            )?,
        })
    }

    pub fn to_unsigned_effects(self) -> UnsignedTransactionEffects {
        UnsignedTransactionEffects {
            transaction_effects_digest: self.transaction_effects_digest,
            effects: self.effects,
            auth_signature: EmptySignInfo {},
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum InputObjectKind {
    // A Move package, must be immutable.
    MovePackage(ObjectID),
    // A Move object, either immutable, or owned mutable.
    ImmOrOwnedMoveObject(ObjectRef),
    // A Move object that's shared and mutable.
    SharedMoveObject(ObjectID),
}

impl InputObjectKind {
    pub fn object_id(&self) -> ObjectID {
        match self {
            Self::MovePackage(id) => *id,
            Self::ImmOrOwnedMoveObject((id, _, _)) => *id,
            Self::SharedMoveObject(id) => *id,
        }
    }

    pub fn version(&self) -> SequenceNumber {
        match self {
            Self::MovePackage(..) => OBJECT_START_VERSION,
            Self::ImmOrOwnedMoveObject((_, version, _)) => *version,
            Self::SharedMoveObject(..) => OBJECT_START_VERSION,
        }
    }

    pub fn object_not_found_error(&self) -> SuiError {
        match *self {
            Self::MovePackage(package_id) => SuiError::DependentPackageNotFound { package_id },
            Self::ImmOrOwnedMoveObject((object_id, _, _)) => SuiError::ObjectNotFound { object_id },
            Self::SharedMoveObject(object_id) => SuiError::ObjectNotFound { object_id },
        }
    }
}

pub struct InputObjects {
    objects: Vec<(InputObjectKind, Object)>,
}

impl InputObjects {
    pub fn new(objects: Vec<(InputObjectKind, Object)>) -> Self {
        Self { objects }
    }

    pub fn len(&self) -> usize {
        self.objects.len()
    }

    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    pub fn filter_owned_objects(&self) -> Vec<ObjectRef> {
        let owned_objects: Vec<_> = self
            .objects
            .iter()
            .filter_map(|(object_kind, object)| match object_kind {
                InputObjectKind::MovePackage(_) => None,
                InputObjectKind::ImmOrOwnedMoveObject(object_ref) => {
                    if object.is_immutable() {
                        None
                    } else {
                        Some(*object_ref)
                    }
                }
                InputObjectKind::SharedMoveObject(_) => None,
            })
            .collect();

        debug!(
            num_mutable_objects = owned_objects.len(),
            "Checked locks and found mutable objects"
        );

        owned_objects
    }

    pub fn filter_shared_objects(&self) -> Vec<ObjectRef> {
        self.objects
            .iter()
            .filter(|(kind, _)| matches!(kind, InputObjectKind::SharedMoveObject(_)))
            .map(|(_, obj)| obj.compute_object_reference())
            .collect()
    }

    pub fn transaction_dependencies(&self) -> BTreeSet<TransactionDigest> {
        self.objects
            .iter()
            .map(|(_, obj)| obj.previous_transaction)
            .collect()
    }

    pub fn mutable_inputs(&self) -> Vec<ObjectRef> {
        self.objects
            .iter()
            .filter_map(|(kind, object)| match kind {
                InputObjectKind::MovePackage(_) => None,
                InputObjectKind::ImmOrOwnedMoveObject(object_ref) => {
                    if object.is_immutable() {
                        None
                    } else {
                        Some(*object_ref)
                    }
                }
                InputObjectKind::SharedMoveObject(_) => Some(object.compute_object_reference()),
            })
            .collect()
    }

    pub fn into_object_map(self) -> BTreeMap<ObjectID, Object> {
        self.objects
            .into_iter()
            .map(|(_, object)| (object.id(), object))
            .collect()
    }
}

impl From<Vec<Object>> for InputObjects {
    fn from(objects: Vec<Object>) -> Self {
        Self::new(
            objects
                .into_iter()
                .map(|o| (o.input_object_kind(), o))
                .collect(),
        )
    }
}

pub struct SignatureAggregator<'a> {
    committee: &'a Committee,
    weight: StakeUnit,
    used_authorities: HashSet<AuthorityName>,
    partial: CertifiedTransaction,
    signature_stash: Vec<(AuthorityName, AuthoritySignature)>,
}

impl<'a> SignatureAggregator<'a> {
    /// Start aggregating signatures for the given value into a certificate.
    pub fn try_new(transaction: Transaction, committee: &'a Committee) -> Result<Self, SuiError> {
        transaction.verify()?;
        Ok(Self::new_unsafe(transaction, committee))
    }

    /// Same as try_new but we don't check the transaction.
    pub fn new_unsafe(transaction: Transaction, committee: &'a Committee) -> Self {
        Self {
            committee,
            weight: 0,
            used_authorities: HashSet::new(),
            partial: CertifiedTransaction::new(committee.epoch, transaction),
            signature_stash: Vec::new(),
        }
    }

    /// Try to append a signature to a (partial) certificate. Returns Some(certificate) if a quorum was reached.
    /// The resulting final certificate is guaranteed to be valid in the sense of `check` below.
    /// Returns an error if the signed value cannot be aggregated.
    pub fn append(
        &mut self,
        authority: AuthorityName,
        signature: AuthoritySignature,
    ) -> Result<Option<CertifiedTransaction>, SuiError> {
        signature.verify(&self.partial.signed_data, authority)?;

        // Check that each authority only appears once.
        fp_ensure!(
            !self.used_authorities.contains(&authority),
            SuiError::CertificateAuthorityReuse
        );
        self.used_authorities.insert(authority);
        // Update weight.
        let voting_rights = self.committee.weight(&authority);
        fp_ensure!(voting_rights > 0, SuiError::UnknownSigner);
        self.weight += voting_rights;
        // Update certificate.

        self.signature_stash.push((authority, signature));

        if self.weight >= self.committee.quorum_threshold() {
            self.partial.auth_sign_info = AuthorityStrongQuorumSignInfo::new_with_signatures(
                self.signature_stash.clone(),
                self.committee,
            )?;
            Ok(Some(self.partial.clone()))
        } else {
            Ok(None)
        }
    }
}

impl CertifiedTransaction {
    pub fn new(epoch: EpochId, transaction: Transaction) -> CertifiedTransaction {
        CertifiedTransaction {
            transaction_digest: transaction.transaction_digest,
            is_verified: false,
            signed_data: transaction.signed_data,
            auth_sign_info: AuthorityStrongQuorumSignInfo::new(epoch),
        }
    }

    pub fn new_with_signatures(
        transaction: Transaction,
        signatures: Vec<(AuthorityName, AuthoritySignature)>,
        committee: &Committee,
    ) -> SuiResult<CertifiedTransaction> {
        Ok(CertifiedTransaction {
            transaction_digest: transaction.transaction_digest,
            is_verified: false,
            signed_data: transaction.signed_data,
            auth_sign_info: AuthorityStrongQuorumSignInfo::new_with_signatures(
                signatures, committee,
            )?,
        })
    }

    pub fn to_transaction(self) -> Transaction {
        Transaction::new(self.signed_data.data, self.signed_data.tx_signature)
    }

    /// Verify the certificate.
    pub fn verify(&self, committee: &Committee) -> Result<(), SuiError> {
        // We use this flag to see if someone has checked this before
        // and therefore we can skip the check. Note that the flag has
        // to be set to true manually, and is not set by calling this
        // "check" function.
        if self.is_verified {
            return Ok(());
        }

        // Add the obligation of the sender signature verification.
        self.verify_sender_signature()?;

        let mut obligation = VerificationObligation::default();
        // Add the obligation of the authority signature verifications.
        let idx = obligation.add_message(&self.signed_data);
        self.auth_sign_info
            .add_to_verification_obligation(committee, &mut obligation, idx)?;

        obligation.verify_all().map(|_| ())
    }

    pub fn epoch(&self) -> EpochId {
        self.auth_sign_info.epoch
    }
}

impl Display for CertifiedTransaction {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut writer = String::new();
        writeln!(writer, "Transaction Hash: {:?}", self.digest())?;
        writeln!(
            writer,
            "Signed Authorities Bitmap : {:?}",
            self.auth_sign_info.signers_map
        )?;
        write!(writer, "{}", &self.signed_data.data.kind)?;
        write!(f, "{}", writer)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ConsensusOutput {
    #[serde(with = "serde_bytes")]
    pub message: Vec<u8>,
    pub sequence_number: SequenceNumber,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ConsensusSync {
    pub sequence_number: SequenceNumber,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ConsensusTransaction {
    /// Encodes an u64 unique tracking id to allow us trace a message between Sui and Narwhal.
    /// Use an byte array instead of u64 to ensure stable serialization.
    pub tracking_id: [u8; 8],
    pub kind: ConsensusTransactionKind,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum ConsensusTransactionKind {
    UserTransaction(Box<CertifiedTransaction>),
    Checkpoint(Box<CheckpointFragment>),
}

impl ConsensusTransaction {
    pub fn new_certificate_message(
        authority: &AuthorityName,
        certificate: CertifiedTransaction,
    ) -> Self {
        let mut hasher = DefaultHasher::new();
        let tx_digest = certificate.digest();
        tx_digest.hash(&mut hasher);
        authority.hash(&mut hasher);
        let tracking_id = hasher.finish().to_be_bytes();
        Self {
            tracking_id,
            kind: ConsensusTransactionKind::UserTransaction(Box::new(certificate)),
        }
    }

    pub fn new_checkpoint_message(fragment: CheckpointFragment) -> Self {
        let mut hasher = DefaultHasher::new();
        let cp_seq = fragment.proposer_sequence_number();
        let proposer = fragment.proposer.auth_signature.authority;
        let other = fragment.other.auth_signature.authority;
        cp_seq.hash(&mut hasher);
        proposer.hash(&mut hasher);
        other.hash(&mut hasher);
        let tracking_id = hasher.finish().to_be_bytes();
        Self {
            tracking_id,
            kind: ConsensusTransactionKind::Checkpoint(Box::new(fragment)),
        }
    }

    pub fn get_tracking_id(&self) -> u64 {
        (&self.tracking_id[..])
            .read_u64::<BigEndian>()
            .unwrap_or_default()
    }

    pub fn verify(&self, committee: &Committee) -> SuiResult<()> {
        match &self.kind {
            ConsensusTransactionKind::UserTransaction(certificate) => certificate.verify(committee),
            ConsensusTransactionKind::Checkpoint(fragment) => fragment.verify(committee),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, schemars::JsonSchema)]
pub enum ExecuteTransactionRequestType {
    ImmediateReturn,
    WaitForTxCert,
    WaitForEffectsCert,
    WaitForLocalExecution,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ExecuteTransactionRequest {
    pub transaction: Transaction,
    pub request_type: ExecuteTransactionRequestType,
}

/// When requested to execute a transaction with WaitForLocalExecution,
/// TransactionOrchestrator attempts to execute this transaction locally
/// after it is finalized. This value represents whether the transaction
/// is confirmed to be executed on this node before the response returns.
pub type IsTransactionExecutedLocally = bool;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum ExecuteTransactionResponse {
    ImmediateReturn,
    TxCert(Box<CertifiedTransaction>),
    // TODO: Change to CertifiedTransactionEffects eventually.
    EffectsCert(
        Box<(
            CertifiedTransaction,
            CertifiedTransactionEffects,
            IsTransactionExecutedLocally,
        )>,
    ),
}

#[derive(Serialize, Deserialize, Clone, Debug, schemars::JsonSchema)]
pub enum QuorumDriverRequestType {
    ImmediateReturn,
    WaitForTxCert,
    WaitForEffectsCert,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct QuorumDriverRequest {
    pub transaction: Transaction,
    pub request_type: QuorumDriverRequestType,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum QuorumDriverResponse {
    ImmediateReturn,
    TxCert(Box<CertifiedTransaction>),
    // TODO: Change to CertifiedTransactionEffects eventually.
    EffectsCert(Box<(CertifiedTransaction, CertifiedTransactionEffects)>),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CommitteeInfoRequest {
    pub epoch: Option<EpochId>,
}

#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema, Debug)]
pub struct CommitteeInfoResponse {
    pub epoch: EpochId,
    pub committee_info: Option<Vec<(AuthorityName, StakeUnit)>>,
    // TODO: We could also return the certified checkpoint that contains this committee.
    // This would allows a client to verify the authenticity of the committee.
}

pub type CommitteeInfoResponseDigest = [u8; 32];

impl CommitteeInfoResponse {
    pub fn digest(&self) -> CommitteeInfoResponseDigest {
        sha3_hash(self)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CommitteeInfo {
    pub epoch: EpochId,
    pub committee_info: Vec<(AuthorityName, StakeUnit)>,
    // TODO: We could also return the certified checkpoint that contains this committee.
    // This would allows a client to verify the authenticity of the committee.
}
