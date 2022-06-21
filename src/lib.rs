mod blockstore;

use crate::blockstore::Blockstore;
use cid::multihash::Code;
use cid::Cid;
use fvm_ipld_encoding::tuple::{Deserialize_tuple, Serialize_tuple};
use fvm_ipld_encoding::{to_vec, CborStore, RawBytes, DAG_CBOR};
use fvm_ipld_hamt::{BytesKey, Hamt};
use fvm_sdk as sdk;
use fvm_sdk::message::NO_DATA_BLOCK_ID;
use fvm_shared::address::Address;
use fvm_shared::bigint::bigint_ser;
use fvm_shared::econ::TokenAmount;
use fvm_shared::ActorID;
use fvm_shared::METHOD_SEND;
use serde::{Deserialize, Serialize};

/// A macro to abort concisely.
/// This should be part of the SDK as it's very handy.
macro_rules! abort {
    ($code:ident, $msg:literal $(, $ex:expr)*) => {
        fvm_sdk::vm::abort(
            fvm_shared::error::ExitCode::$code.value(),
            Some(format!($msg, $($ex,)*).as_str()),
        )
    };
}

#[derive(Serialize, Deserialize, Debug)]
pub struct BountyKey {
    pub piece_cid: Cid,
    pub address: Address,
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
pub struct BountyValue {
    #[serde(with = "bigint_ser")]
    pub amount: TokenAmount,
}

/// The state object.
#[derive(Serialize_tuple, Deserialize_tuple, Clone, Debug)]
pub struct State {
    pub trusted_address: Address,
    pub bounties_map: Cid,
}

/// We should probably have a derive macro to mark an object as a state object,
/// and have load and save methods automatically generated for them as part of a
/// StateObject trait (i.e. impl StateObject for State).
impl State {
    pub fn load() -> Self {
        // First, load the current state root.
        let root = match sdk::sself::root() {
            Ok(root) => root,
            Err(err) => abort!(USR_ILLEGAL_STATE, "failed to get root: {:?}", err),
        };

        // Load the actor state from the state tree.
        match Blockstore.get_cbor::<Self>(&root) {
            Ok(Some(state)) => state,
            Ok(None) => abort!(USR_ILLEGAL_STATE, "state does not exist"),
            Err(err) => abort!(USR_ILLEGAL_STATE, "failed to get state: {}", err),
        }
    }

    pub fn save(&self) -> Cid {
        let serialized = match to_vec(self) {
            Ok(s) => s,
            Err(err) => abort!(USR_SERIALIZATION, "failed to serialize state: {:?}", err),
        };
        let cid = match sdk::ipld::put(Code::Blake2b256.into(), 32, DAG_CBOR, serialized.as_slice())
        {
            Ok(cid) => cid,
            Err(err) => abort!(USR_SERIALIZATION, "failed to store initial state: {:}", err),
        };
        if let Err(err) = sdk::sself::set_root(&cid) {
            abort!(USR_ILLEGAL_STATE, "failed to set root ciid: {:}", err);
        }
        cid
    }
}

/// The actor's WASM entrypoint. It takes the ID of the parameters block,
/// and returns the ID of the return value block, or NO_DATA_BLOCK_ID if no
/// return value.
///
/// Should probably have macros similar to the ones on fvm.filecoin.io snippets.
/// Put all methods inside an impl struct and annotate it with a derive macro
/// that handles state serde and dispatch.
#[no_mangle]
pub fn invoke(params: u32) -> u32 {
    // Conduct method dispatch. Handle input parameters and return data.
    let ret: Option<RawBytes> = match sdk::message::method_number() {
        1 => constructor(params),
        2 => post_bounty(params),
        3 => list_bounties(),
        4 => lookup_bounty(params),
        5 => award_bounty(params),
        _ => abort!(USR_UNHANDLED_MESSAGE, "unrecognized method"),
    };

    // Insert the return data block if necessary, and return the correct
    // block ID.
    match ret {
        None => NO_DATA_BLOCK_ID,
        Some(v) => match sdk::ipld::put_block(DAG_CBOR, v.bytes()) {
            Ok(id) => id,
            Err(err) => abort!(USR_SERIALIZATION, "failed to store return value: {}", err),
        },
    }
}

/// The constructor populates the initial state.
///
/// Method num 1. This is part of the Filecoin calling convention.
/// InitActor#Exec will call the constructor on method_num = 1.
pub fn constructor(params: u32) -> Option<RawBytes> {
    let params = sdk::message::params_raw(params).unwrap().1;
    let trusted_address = Address::from_bytes(&params).unwrap();

    // This constant should be part of the SDK.
    const INIT_ACTOR_ADDR: ActorID = 1;

    // Should add SDK sugar to perform ACL checks more succinctly.
    // i.e. the equivalent of the validate_* builtin-actors runtime methods.
    // https://github.com/filecoin-project/builtin-actors/blob/master/actors/runtime/src/runtime/fvm.rs#L110-L146
    if sdk::message::caller() != INIT_ACTOR_ADDR {
        abort!(USR_FORBIDDEN, "constructor invoked by non-init actor");
    }

    let mut state = State {
        trusted_address,
        bounties_map: Cid::default(),
    };
    let mut bounties: Hamt<Blockstore, BountyValue, BytesKey> = Hamt::new(Blockstore);
    let bounties_cid = match bounties.flush() {
        Ok(map) => map,
        Err(_e) => abort!(USR_ILLEGAL_STATE, "failed to create bounties hamt"),
    };
    state.bounties_map = bounties_cid;
    state.save();
    None
}

#[derive(Debug, Deserialize_tuple)]
pub struct PostBountyParams {
    pub piece_cid: Cid,
    pub address: Address,
}

/// Method num 2.
pub fn post_bounty(params: u32) -> Option<RawBytes> {
    let params = sdk::message::params_raw(params).unwrap().1;
    let params = RawBytes::new(params);
    let params: PostBountyParams = params.deserialize().unwrap();

    let mut state = State::load();

    let mut bounties =
        match Hamt::<Blockstore, BountyValue, BytesKey>::load(&state.bounties_map, Blockstore) {
            Ok(map) => map,
            Err(err) => abort!(USR_ILLEGAL_STATE, "failed to load bounties hamt: {:?}", err),
        };

    let key = BountyKey {
        piece_cid: params.piece_cid,
        address: params.address,
    };
    let raw_bytes = RawBytes::serialize(&key).unwrap();
    let bytes = raw_bytes.bytes();
    let key = BytesKey::from(bytes);

    let mut amount = match bounties.get(&key) {
        Ok(Some(bounty_value)) => bounty_value.amount.clone(),
        Ok(None) => TokenAmount::from(0),
        Err(err) => abort!(
            USR_ILLEGAL_STATE,
            "failed to query hamt when getting bounty balance: {:?}",
            err
        ),
    };
    amount += sdk::message::value_received();

    if amount > TokenAmount::from(0) {
        let bounty_value = BountyValue { amount: amount };
        bounties.set(key, bounty_value).unwrap();

        // Flush the HAMT to generate the new root CID to update the actor's state.
        let cid = match bounties.flush() {
            Ok(cid) => cid,
            Err(err) => abort!(USR_ILLEGAL_STATE, "failed to flush hamt: {:?}", err),
        };

        // Update the actor's state.
        state.bounties_map = cid;
        state.save();
    }

    None
}

#[derive(Debug, Serialize)]
pub struct PostedBounty {
    pub piece_cid: Cid,
    pub address: Address,
    #[serde(with = "bigint_ser")]
    pub amount: TokenAmount,
}

/// Method num 3.
pub fn list_bounties() -> Option<RawBytes> {
    let mut bounties_vec = Vec::new();

    let state = State::load();
    let bounties =
        match Hamt::<Blockstore, BountyValue, BytesKey>::load(&state.bounties_map, Blockstore) {
            Ok(map) => map,
            Err(err) => abort!(USR_ILLEGAL_STATE, "failed to load bounties hamt: {:?}", err),
        };
    bounties
        .for_each(|k, v: &BountyValue| {
            let raw_bytes = RawBytes::new(k.as_slice().to_vec());
            let key: BountyKey = raw_bytes.deserialize().unwrap();
            let posted_bounty = PostedBounty {
                piece_cid: key.piece_cid,
                address: key.address,
                amount: v.amount.clone(),
            };
            bounties_vec.push(posted_bounty);
            Ok(())
        })
        .unwrap();

    Some(RawBytes::serialize(&bounties_vec).unwrap())
}

/// Method num 4.
pub fn lookup_bounty(params: u32) -> Option<RawBytes> {
    let params = sdk::message::params_raw(params).unwrap().1;
    let params = RawBytes::new(params);
    let params: PostBountyParams = params.deserialize().unwrap();

    let state = State::load();
    let bounties =
        match Hamt::<Blockstore, BountyValue, BytesKey>::load(&state.bounties_map, Blockstore) {
            Ok(map) => map,
            Err(err) => abort!(USR_ILLEGAL_STATE, "failed to load bounties hamt: {:?}", err),
        };

    let key = BountyKey {
        piece_cid: params.piece_cid,
        address: params.address,
    };
    let raw_bytes = RawBytes::serialize(&key).unwrap();
    let bytes = raw_bytes.bytes();
    let key = BytesKey::from(bytes);
    let amount = match bounties.get(&key) {
        Ok(Some(bounty_value)) => bounty_value.amount.clone(),
        Ok(None) => TokenAmount::from(0),
        Err(err) => abort!(
            USR_ILLEGAL_STATE,
            "failed to query hamt when getting bounty balance: {:?}",
            err
        ),
    };
    let bounty_value = BountyValue { amount: amount };
    Some(RawBytes::serialize(&bounty_value).unwrap())
}

#[derive(Debug, Deserialize_tuple)]
pub struct AwardBountyParams {
    pub piece_cid: Cid,
    pub address: Address,
    pub payout_address: Address,
}

/// Method num 5.
pub fn award_bounty(params: u32) -> Option<RawBytes> {
    let params = sdk::message::params_raw(params).unwrap().1;
    let params = RawBytes::new(params);
    let params: AwardBountyParams = params.deserialize().unwrap();

    let mut state = State::load();

    let caller = sdk::message::caller();
    let address = Address::new_id(caller);
    if state.trusted_address != address.clone() {
        abort!(
            USR_FORBIDDEN,
            "caller not trusted {:?} != {:?} (trusted)",
            address,
            &state.trusted_address
        );
    }

    let mut bounties =
        match Hamt::<Blockstore, BountyValue, BytesKey>::load(&state.bounties_map, Blockstore) {
            Ok(map) => map,
            Err(err) => abort!(USR_ILLEGAL_STATE, "failed to load bounties hamt: {:?}", err),
        };

    let key = BountyKey {
        piece_cid: params.piece_cid,
        address: params.address,
    };
    let raw_bytes = RawBytes::serialize(&key).unwrap();
    let bytes = raw_bytes.bytes();
    let key = BytesKey::from(bytes);

    let amount = match bounties.get(&key) {
        Ok(Some(bounty_value)) => bounty_value.amount.clone(),
        Ok(None) => TokenAmount::from(0),
        Err(err) => abort!(
            USR_ILLEGAL_STATE,
            "failed to query hamt when getting bounty balance: {:?}",
            err
        ),
    };

    if amount > TokenAmount::from(0) {
        let send_params = RawBytes::default();
        let _receipt =
            fvm_sdk::send::send(&params.payout_address, METHOD_SEND, send_params, amount).unwrap();

        bounties.delete(&key).unwrap();

        // Flush the HAMT to generate the new root CID to update the actor's state.
        let cid = match bounties.flush() {
            Ok(cid) => cid,
            Err(err) => abort!(USR_ILLEGAL_STATE, "failed to flush hamt: {:?}", err),
        };

        // Update the actor's state.
        state.bounties_map = cid;
        state.save();
    }

    None
}
