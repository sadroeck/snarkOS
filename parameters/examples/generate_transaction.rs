// Copyright (C) 2019-2021 Aleo Systems Inc.
// This file is part of the snarkOS library.

// The snarkOS library is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// The snarkOS library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with the snarkOS library. If not, see <https://www.gnu.org/licenses/>.

use snarkos_consensus::{Consensus, ConsensusParameters, MerkleTreeLedger};
use snarkos_storage::{Ledger, LedgerStorage};
use snarkvm_algorithms::{merkle_tree::MerkleTree, traits::LoadableMerkleParameters, MerkleParameters, CRH};
use snarkvm_dpc::{
    testnet1::{instantiated::*, payload::Payload as RecordPayload, DPC},
    Account,
    AccountAddress,
    AccountScheme,
    DPCComponents,
    DPCError,
    DPCScheme,
    LedgerError,
    Network,
    Storage,
    TransactionScheme,
};
use snarkvm_parameters::{LedgerMerkleTreeParameters, Parameter};
use snarkvm_posw::PoswMarlin;
use snarkvm_utilities::{
    bytes::{FromBytes, ToBytes},
    to_bytes,
};

use parking_lot::RwLock;
use rand::{thread_rng, Rng};
use std::{
    fs::{self, File},
    io::{Result as IoResult, Write},
    marker::PhantomData,
    path::Path,
    str::FromStr,
    sync::Arc,
};

/// Generate a blank ledger to facilitate generation of the genesis block
fn empty_ledger<T: TransactionScheme, P: LoadableMerkleParameters, S: Storage>(
    parameters: Arc<P>,
    path: &Path,
) -> Result<Ledger<T, P, S>, LedgerError> {
    fs::create_dir_all(&path).map_err(|err| LedgerError::Message(err.to_string()))?;
    let storage = S::open(Some(path), None)
        .map(|storage| storage)
        .map_err(|err| LedgerError::Message(err.to_string()))?;

    let leaves: &[[u8; 32]] = &[];
    let cm_merkle_tree = MerkleTree::<P>::new(parameters.clone(), &leaves[..])?;

    Ok(Ledger {
        current_block_height: Default::default(),
        storage,
        cm_merkle_tree: RwLock::new(cm_merkle_tree),
        ledger_parameters: parameters,
        _transaction: PhantomData,
    })
}

pub fn generate<S: Storage>(recipient: &str, value: u64, network_id: u8, file_name: &str) -> Result<Vec<u8>, DPCError> {
    let rng = &mut thread_rng();

    let parameters = ConsensusParameters {
        max_block_size: 1_000_000_000usize,
        max_nonce: u32::max_value(),
        target_block_time: 10i64,
        network_id: Network::from_network_id(network_id),
        verifier: PoswMarlin::verify_only().expect("could not instantiate PoSW verifier"),
        authorized_inner_snark_ids: vec![],
    };
    let public_parameters = <InstantiatedDPC as DPCScheme<MerkleTreeLedger<S>>>::NetworkParameters::load(false)?;

    let recipient = AccountAddress::<Components>::from_str(&recipient)?;

    let crh_parameters = <MerkleTreeCRH as CRH>::Parameters::read(&LedgerMerkleTreeParameters::load_bytes()?[..])
        .expect("read bytes as hash for MerkleParameters in ledger");
    let merkle_tree_hash_parameters = <CommitmentMerkleParameters as MerkleParameters>::H::from(crh_parameters);

    // Instantiate an empty ledger

    let ledger_parameters = Arc::new(From::from(merkle_tree_hash_parameters));
    let mut path = std::env::temp_dir();
    let random_path: usize = rng.gen();
    path.push(format!("./empty_ledger-{}", random_path));
    let ledger = Arc::new(empty_ledger::<_, _, S>(ledger_parameters, &path)?);

    let consensus = Consensus {
        parameters,
        public_parameters,
        ledger,
        memory_pool: Default::default(),
    };

    let noop_program_vk_hash = consensus
        .public_parameters
        .system_parameters
        .program_verification_key_crh
        .hash(&to_bytes![
            consensus
                .public_parameters
                .noop_program_snark_parameters
                .verification_key
        ]?)?;
    let noop_program_id = to_bytes![noop_program_vk_hash]?;

    // Generate a new account that owns the dummy input records
    let dummy_account = Account::new(
        &consensus.public_parameters.system_parameters.account_signature,
        &consensus.public_parameters.system_parameters.account_commitment,
        &consensus.public_parameters.system_parameters.account_encryption,
        rng,
    )?;

    // Generate dummy input records

    let old_account_private_keys = vec![dummy_account.private_key.clone(); Components::NUM_INPUT_RECORDS];
    let mut old_records = Vec::with_capacity(Components::NUM_INPUT_RECORDS);
    for i in 0..Components::NUM_INPUT_RECORDS {
        let old_sn_nonce = consensus
            .public_parameters
            .system_parameters
            .serial_number_nonce
            .hash(&[64u8 + (i as u8); 1])?;
        let old_record = DPC::generate_record(
            &consensus.public_parameters.system_parameters,
            old_sn_nonce.clone(),
            dummy_account.address.clone(),
            true, // The input record is dummy
            0,
            RecordPayload::default(),
            noop_program_id.clone(),
            noop_program_id.clone(),
            rng,
        )?;
        old_records.push(old_record);
    }

    // Construct new records

    let new_record_owners = vec![recipient; Components::NUM_OUTPUT_RECORDS];
    let new_payloads = vec![RecordPayload::default(); Components::NUM_OUTPUT_RECORDS];
    let new_birth_program_ids = vec![noop_program_id.clone(); Components::NUM_OUTPUT_RECORDS];
    let new_death_program_ids = vec![noop_program_id; Components::NUM_OUTPUT_RECORDS];

    let mut new_is_dummy_flags = vec![false];
    new_is_dummy_flags.extend(vec![true; Components::NUM_OUTPUT_RECORDS - 1]);

    let mut new_values = vec![value];
    new_values.extend(vec![0; Components::NUM_OUTPUT_RECORDS - 1]);

    // Memo is a dummy for now

    let memo: [u8; 32] = rng.gen();

    // Generate the transaction
    let (records, transaction) = consensus
        .create_transaction(
            old_records,
            old_account_private_keys,
            new_record_owners,
            new_birth_program_ids,
            new_death_program_ids,
            new_is_dummy_flags,
            new_values,
            new_payloads,
            memo,
            rng,
        )
        .unwrap();

    let transaction_bytes = to_bytes![transaction]?;

    let size = transaction_bytes.len();
    println!("{}\n\tsize - {}\n", file_name, size);

    for (i, record) in records.iter().enumerate() {
        let record_bytes = to_bytes![record]?;
        println!("record {}: {:?}\n", i, hex::encode(record_bytes));
    }

    Ok(transaction_bytes)
}

pub fn store<P: AsRef<Path>>(path: P, bytes: &[u8]) -> IoResult<()> {
    let mut file = File::create(path)?;
    file.write_all(&bytes)?;

    Ok(())
}

pub fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 5 {
        println!("Invalid number of arguments.  Given: {} - Required: 4", args.len() - 1);
        return;
    }

    let recipient = &args[1];
    let balance = args[2].parse::<u64>().unwrap();
    let network_id = args[3].parse::<u8>().unwrap();
    let file_name = &args[4];

    let bytes = generate::<LedgerStorage>(recipient, balance, network_id, file_name).unwrap();
    store(file_name, &bytes).unwrap();
}
