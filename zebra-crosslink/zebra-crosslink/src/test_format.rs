use static_assertions::*;
use std::{io::Write, mem::align_of, mem::size_of};
use zebra_chain::serialization::{ZcashDeserialize, ZcashSerialize, SerializationError};
use zerocopy::*;
use zerocopy_derive::*;

use zcash_primitives::bft::*;

pub struct BftBlockAndFatPointerToItWrap(pub BftBlockAndFatPointerToIt);
impl ZcashDeserialize for BftBlockAndFatPointerToItWrap {
    fn zcash_deserialize<R: std::io::Read>(mut reader: R) -> Result<Self, SerializationError> { // SerializationError> {
        Ok(Self(BftBlockAndFatPointerToIt::zcash_deserialize(&mut reader)?))
    }
}
impl ZcashSerialize for BftBlockAndFatPointerToItWrap {
    fn zcash_serialize<W: std::io::Write>(&self, mut writer: W) -> Result<(), std::io::Error> {
        self.0.zcash_serialize(&mut writer)?;
        Ok(())
    }
}


#[repr(C)]
#[derive(Immutable, KnownLayout, IntoBytes, FromBytes)]
pub struct TFHdr {
    pub magic: [u8; 8],
    pub instrs_o: u64,
    pub instrs_n: u32,
    pub instr_size: u32, // used as stride
}

#[repr(C)]
#[derive(Clone, Copy, Immutable, IntoBytes, FromBytes)]
pub struct TFSlice {
    pub o: u64,
    pub size: u64,
}

impl TFSlice {
    pub fn as_val(self) -> [u64; 2] {
        [self.o, self.size]
    }

    pub fn as_byte_slice_in(self, bytes: &[u8]) -> &[u8] {
        &bytes[self.o as usize..(self.o + self.size) as usize]
    }
}

impl From<&[u64; 2]> for TFSlice {
    fn from(val: &[u64; 2]) -> TFSlice {
        TFSlice {
            o: val[0],
            size: val[1],
        }
    }
}

type TFInstrKind = u32;

#[repr(C)]
#[derive(Clone, Copy, Immutable, IntoBytes, FromBytes)]
pub struct TFInstr {
    pub kind: TFInstrKind,
    pub flags: u32,
    pub data: TFSlice,
    pub val: [u64; 2],
}

pub const TEST_STAKE_IGNORED: u64 = u64::MAX;

static TF_INSTR_KIND_STRS: [&str; TFInstr::COUNT as usize] = {
    let mut strs = [""; TFInstr::COUNT as usize];
    strs[TFInstr::LOAD_POW as usize] = "LOAD_POW";
    strs[TFInstr::LOAD_POS as usize] = "LOAD_POS";
    strs[TFInstr::SET_PARAMS as usize] = "SET_PARAMS";
    strs[TFInstr::EXPECT_POW_CHAIN_LENGTH as usize] = "EXPECT_POW_CHAIN_LENGTH";
    strs[TFInstr::EXPECT_POS_CHAIN_LENGTH as usize] = "EXPECT_POS_CHAIN_LENGTH";
    strs[TFInstr::EXPECT_POW_BLOCK_FINALITY as usize] = "EXPECT_POW_BLOCK_FINALITY";
    strs[TFInstr::ROSTER_FORCE_INCLUDE as usize] = "ROSTER_FORCE_INCLUDE";
    strs[TFInstr::EXPECT_ROSTER_INCLUDES as usize] = "EXPECT_ROSTER_INCLUDES";

    const_assert!(TFInstr::COUNT == 8);
    strs
};

impl TFInstr {
    // NOTE: we want to deal with unknown values at the *application* layer, not the
    // (de)serialization layer.
    // TODO: there may be a crate that makes an enum feasible here
    pub const LOAD_POW: TFInstrKind = 0;
    pub const LOAD_POS: TFInstrKind = 1;
    pub const SET_PARAMS: TFInstrKind = 2;
    pub const EXPECT_POW_CHAIN_LENGTH: TFInstrKind = 3;
    pub const EXPECT_POS_CHAIN_LENGTH: TFInstrKind = 4;
    pub const EXPECT_POW_BLOCK_FINALITY: TFInstrKind = 5;
    pub const ROSTER_FORCE_INCLUDE: TFInstrKind = 6;
    pub const EXPECT_ROSTER_INCLUDES: TFInstrKind = 7;
    pub const COUNT: TFInstrKind = 8;

    pub fn str_from_kind(kind: TFInstrKind) -> &'static str {
        let kind = kind as usize;
        if kind < TF_INSTR_KIND_STRS.len() {
            TF_INSTR_KIND_STRS[kind]
        } else {
            "<unknown>"
        }
    }

    pub fn string_from_instr(bytes: &[u8], instr: &TFInstr) -> String {
        let mut str = Self::str_from_kind(instr.kind).to_string();
        str += " (";

        match tf_read_instr(&bytes, instr) {
            Some(TestInstr::LoadPoW(block)) => {
                str += &format!(
                    "{} - {}, parent: {}",
                    block.coinbase_height().unwrap().0,
                    block.hash(),
                    block.header.previous_block_hash
                )
            }
            Some(TestInstr::LoadPoS((block, fat_ptr))) => {
                str += &format!(
                    "{}, snapshot: {}, hdrs: [{} .. {}]",
                    block.blake3_hash(),
                    block.snapshot_block_hash(),
                    BlockHash::from_header_data(&block.headers[0]),
                    BlockHash::from_header_data(block.headers.last().unwrap())
                )
            }
            Some(TestInstr::SetParams(params)) => {
                str += &format!(
                    "{} {:?}",
                    params.bc_confirmation_depth_sigma, params.bootstrap
                )
            }
            Some(TestInstr::ExpectPoWChainLength(h)) => str += &h.to_string(),
            Some(TestInstr::ExpectPoSChainLength(h)) => str += &h.to_string(),
            Some(TestInstr::ExpectPoWBlockFinality(hash, f)) => {
                str += &format!("{} => {:?}", hash, f)
            }
            Some(TestInstr::ExpectRosterIncludes(pub_key, stake)) => {
                str += &format!("{} => {}", PubKeyID(pub_key), stake)
            }
            Some(TestInstr::RosterForceInclude(pub_key, stake)) => {
                str += &format!("{} => {}", PubKeyID(pub_key), stake)
            }
            None => {}
        }

        str += ")";

        if instr.flags != 0 {
            str += " [";
            if (instr.flags & SHOULD_FAIL) != 0 {
                str += " SHOULD_FAIL";
            }
            str += " ]";
        }

        str
    }

    pub fn data_slice<'a>(&self, bytes: &'a [u8]) -> &'a [u8] {
        self.data.as_byte_slice_in(bytes)
    }
}

// Flags
pub const SHOULD_FAIL: u32 = 1 << 0;

pub struct TF {
    pub instrs: Vec<TFInstr>,
    pub data: Vec<u8>,
}

pub const TF_NOT_YET_FINALIZED: u64 = 0;
pub const TF_FINALIZED: u64 = 1;
pub const TF_CANT_BE_FINALIZED: u64 = 2;

pub fn finality_from_val(val: &[u64; 2]) -> Option<TFLBlockFinality> {
    if (val[0] == 0) {
        None
    } else {
        match (val[1]) {
            test_format::TF_NOT_YET_FINALIZED => Some(TFLBlockFinality::NotYetFinalized),
            test_format::TF_FINALIZED => Some(TFLBlockFinality::Finalized),
            test_format::TF_CANT_BE_FINALIZED => Some(TFLBlockFinality::CantBeFinalized),
            _ => panic!("unexpected finality value"),
        }
    }
}

pub fn val_from_finality(val: Option<TFLBlockFinality>) -> [u64; 2] {
    match (val) {
        Some(TFLBlockFinality::NotYetFinalized) => [1u64, test_format::TF_NOT_YET_FINALIZED],
        Some(TFLBlockFinality::Finalized) => [1u64, test_format::TF_FINALIZED],
        Some(TFLBlockFinality::CantBeFinalized) => [1u64, test_format::TF_CANT_BE_FINALIZED],
        None => [0u64; 2],
    }
}

impl TF {
    pub fn new(params: &ZcashCrosslinkParameters) -> TF {
        let mut tf = TF {
            instrs: Vec::new(),
            data: Vec::new(),
        };

        // Enforce that every parameter is written: adding a member fails to compile here.
        let ZcashCrosslinkParameters {
            bc_confirmation_depth_sigma,
            bootstrap,
        } = *params;
        tf.push_instr_ex(
            TFInstr::SET_PARAMS,
            0,
            &bootstrap_to_bytes(bootstrap),
            [bc_confirmation_depth_sigma, 0],
        );

        tf
    }

    pub fn push_serialize<Z: ZcashSerialize>(&mut self, z: &Z) -> TFSlice {
        let bgn = (size_of::<TFHdr>() + self.data.len()) as u64;
        z.zcash_serialize(&mut self.data);
        let end = (size_of::<TFHdr>() + self.data.len()) as u64;

        TFSlice {
            o: bgn,
            size: end - bgn,
        }
    }

    pub fn push_data(&mut self, bytes: &[u8]) -> TFSlice {
        let result = TFSlice {
            o: (size_of::<TFHdr>() + self.data.len()) as u64,
            size: bytes.len() as u64,
        };
        self.data.write_all(bytes);
        result
    }

    pub fn push_instr_ex(&mut self, kind: TFInstrKind, flags: u32, data: &[u8], val: [u64; 2]) {
        let data = self.push_data(data);
        self.instrs.push(TFInstr {
            kind,
            flags,
            data,
            val,
        });
    }

    pub fn push_instr(&mut self, kind: TFInstrKind, data: &[u8]) {
        self.push_instr_ex(kind, 0, data, [0; 2])
    }

    pub fn push_instr_val(&mut self, kind: TFInstrKind, val: [u64; 2]) {
        self.push_instr_ex(kind, 0, &[0; 0], val)
    }

    pub fn push_instr_serialize_ex<Z: ZcashSerialize>(
        &mut self,
        kind: TFInstrKind,
        flags: u32,
        data: &Z,
        val: [u64; 2],
    ) {
        let data = self.push_serialize(data);
        self.instrs.push(TFInstr {
            kind,
            flags,
            data,
            val,
        });
    }

    pub fn push_instr_serialize<Z: ZcashSerialize>(&mut self, kind: TFInstrKind, data: &Z) {
        self.push_instr_serialize_ex(kind, 0, data, [0; 2])
    }

    pub fn push_instr_load_pow(&mut self, data: &Block, flags: u32) {
        self.push_instr_serialize_ex(TFInstr::LOAD_POW, flags, data, [0; 2])
    }
    pub fn push_instr_load_pow_bytes(&mut self, data: &[u8], flags: u32) {
        self.push_instr_ex(TFInstr::LOAD_POW, flags, data, [0; 2])
    }

    pub fn push_instr_load_pos(&mut self, data: &BftBlockAndFatPointerToItWrap, flags: u32) {
        self.push_instr_serialize_ex(TFInstr::LOAD_POS, flags, data, [0; 2])
    }
    pub fn push_instr_load_pos_bytes(&mut self, data: &[u8], flags: u32) {
        self.push_instr_ex(TFInstr::LOAD_POS, flags, data, [0; 2])
    }

    pub fn push_instr_expect_pow_chain_length(&mut self, length: usize, flags: u32) {
        self.push_instr_ex(
            TFInstr::EXPECT_POW_CHAIN_LENGTH,
            flags,
            &[0; 0],
            [length as u64, 0],
        )
    }

    pub fn push_instr_expect_pos_chain_length(&mut self, length: usize, flags: u32) {
        self.push_instr_ex(
            TFInstr::EXPECT_POS_CHAIN_LENGTH,
            flags,
            &[0; 0],
            [length as u64, 0],
        )
    }

    pub fn push_instr_expect_pow_block_finality(
        &mut self,
        pow_hash: &ZebBlockHash,
        finality: Option<TFLBlockFinality>,
        flags: u32,
    ) {
        self.push_instr_ex(
            TFInstr::EXPECT_POW_BLOCK_FINALITY,
            flags,
            &pow_hash.0,
            test_format::val_from_finality(finality),
        )
    }

    pub fn push_instr_roster_force_include(&mut self, pub_key: [u8; 32], stake: u64, flags: u32) {
        self.push_instr_ex(TFInstr::ROSTER_FORCE_INCLUDE, flags, &pub_key, [stake, 0])
    }

    pub fn push_instr_expect_roster_includes(&mut self, pub_key: [u8; 32], stake: u64, flags: u32) {
        self.push_instr_ex(TFInstr::EXPECT_ROSTER_INCLUDES, flags, &pub_key, [stake, 0])
    }

    fn is_a_power_of_2(v: usize) -> bool {
        v != 0 && ((v & (v - 1)) == 0)
    }

    fn align_up(v: usize, mut align: usize) -> usize {
        assert!(Self::is_a_power_of_2(align));
        align -= 1;
        (v + align) & !align
    }

    pub fn write<W: std::io::Write>(&self, writer: &mut W) -> bool {
        let instrs_o_unaligned = size_of::<TFHdr>() + self.data.len();
        let instrs_o = Self::align_up(instrs_o_unaligned, align_of::<TFInstr>());
        let hdr = TFHdr {
            magic: "ZECCLTF0".as_bytes().try_into().unwrap(),
            instrs_o: instrs_o as u64,
            instrs_n: self.instrs.len() as u32,
            instr_size: size_of::<TFInstr>() as u32,
        };
        writer
            .write_all(hdr.as_bytes())
            .expect("writing shouldn't fail");
        writer
            .write_all(&self.data)
            .expect("writing shouldn't fail");

        if instrs_o > instrs_o_unaligned {
            const ALIGN_0S: [u8; align_of::<TFInstr>()] = [0u8; align_of::<TFInstr>()];
            let align_size = instrs_o - instrs_o_unaligned;
            let align_bytes = &ALIGN_0S[..align_size];
            writer.write_all(align_bytes);
        }
        writer
            .write_all(self.instrs.as_bytes())
            .expect("writing shouldn't fail");

        true
    }

    pub fn write_to_file(&self, path: &std::path::Path) -> bool {
        if let Ok(mut file) = std::fs::File::create(path) {
            self.write(&mut file)
        } else {
            false
        }
    }

    pub fn write_to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        self.write(&mut bytes);
        bytes
    }

    // Simple version, all in one go... for large files we'll want to break this up; get hdr &
    // get/stream instrs, then read data as needed
    pub fn read_from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let tf_hdr = match TFHdr::ref_from_prefix(&bytes[0..]) {
            Ok((hdr, _)) => hdr,
            Err(err) => return Err(err.to_string()),
        };

        let read_instrs = <[TFInstr]>::ref_from_prefix_with_elems(
            &bytes[tf_hdr.instrs_o as usize..],
            tf_hdr.instrs_n as usize,
        );

        let instrs = match read_instrs {
            Ok((instrs, _)) => instrs,
            Err(err) => return Err(err.to_string()),
        };

        let data = &bytes[size_of::<TFHdr>()..tf_hdr.instrs_o as usize];

        // TODO: just use slices, don't copy to vectors
        let tf = TF {
            instrs: instrs.to_vec(),
            data: data.to_vec(),
        };

        Ok(tf)
    }

    pub fn read_from_file(path: &std::path::Path) -> Result<(Vec<u8>, Self), String> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) => return Err(err.to_string()),
        };

        Self::read_from_bytes(&bytes).map(|tf| (bytes, tf))
    }
}

// TODO: macro for a stringified condition
fn test_check(flags: u32, condition: bool, message: &str) {
    let should_succeed = (flags & SHOULD_FAIL) == 0;
    const SUCCESS_STRS: [&str; 2] = ["fail", "succeed"];

    if condition != should_succeed {
        let test_instr_i = *TEST_INSTR_C.lock().unwrap();
        TEST_FAILED_INSTR_IDXS.lock().unwrap().push((test_instr_i, message.to_string()));

        match *TEST_CHECK_ASSERT.lock().unwrap() {
            0 => {},
            1 => error!(
                "test check should {} but actually {}ed, message:\n{}",
                SUCCESS_STRS[should_succeed as usize],
                SUCCESS_STRS[!should_succeed as usize],
                message
            ),
            _ => panic!(
                "test check should {} but actually {}ed (and TEST_CHECK_ASSERT enabled), message:\n{}",
                SUCCESS_STRS[should_succeed as usize],
                SUCCESS_STRS[!should_succeed as usize],
                message
            ),
        }
    }
}

use crate::*;

/// Parameters for scenarios that feed BFT blocks in directly, which is every scenario that is not
/// testing the bootstrap itself. A file with no `SET_PARAMS` is read as these: every scenario was
/// written that way before the bootstrap became a parameter.
pub const HARNESS_PARAMETERS: ZcashCrosslinkParameters = ZcashCrosslinkParameters {
    bootstrap: BftBootstrap::Supplied,
    // Sigma is pinned here rather than inherited from `PROTOTYPE_PARAMETERS`. The
    // scenes in the test suite are hand-built at specific heights: a BFT block carries exactly
    // sigma headers, and the PoW block that cites it has to sit at least sigma + 1 above the
    // block that certificate finalizes. Inheriting sigma would silently invalidate every one of
    // those scenes the moment the network parameter moved, which is not what changing a network
    // parameter should mean. The rules under test do not depend on sigma's value; the live
    // network's value is exercised on a testnet, not here.
    bc_confirmation_depth_sigma: 3,
};

// `SET_PARAMS` carries sigma in `val[0]`, and the bootstrap in its data. `val[1]` is written as
// zero and never read: it used to carry the Book's `L`, which Zebra Crosslink does not have.
const TF_BOOTSTRAP_SUPPLIED: u8 = 0;
const TF_BOOTSTRAP_FROM_CHAIN: u8 = 1;

fn bootstrap_to_bytes(bootstrap: BftBootstrap) -> Vec<u8> {
    match bootstrap {
        BftBootstrap::Supplied => vec![TF_BOOTSTRAP_SUPPLIED],
        BftBootstrap::FromChain { roster_height, activation_height } => {
            let mut bytes = vec![TF_BOOTSTRAP_FROM_CHAIN];
            bytes.extend_from_slice(&roster_height.to_le_bytes());
            bytes.extend_from_slice(&activation_height.to_le_bytes());
            bytes
        }
    }
}

fn bootstrap_from_bytes(bytes: &[u8]) -> Option<BftBootstrap> {
    match bytes {
        [TF_BOOTSTRAP_SUPPLIED] => Some(BftBootstrap::Supplied),
        [TF_BOOTSTRAP_FROM_CHAIN, r0, r1, r2, r3, a0, a1, a2, a3] => Some(BftBootstrap::FromChain {
            roster_height: u32::from_le_bytes([*r0, *r1, *r2, *r3]),
            activation_height: u32::from_le_bytes([*a0, *a1, *a2, *a3]),
        }),
        _ => None,
    }
}

/// The Crosslink parameters a test file's node must run with: its leading `SET_PARAMS`, or
/// [`HARNESS_PARAMETERS`] if it has none. They are consensus parameters, so the harness builds the
/// network with them before the node boots instead of applying them when the instruction runs.
pub fn crosslink_parameters_for_test(bytes: &[u8]) -> ZcashCrosslinkParameters {
    let Ok(tf) = TF::read_from_bytes(bytes) else {
        return HARNESS_PARAMETERS;
    };
    let Some(first) = tf.instrs.first() else {
        return HARNESS_PARAMETERS;
    };
    if first.kind != TFInstr::SET_PARAMS {
        return HARNESS_PARAMETERS;
    }
    // A malformed SET_PARAMS falls back here, then fails loudly when the instruction is read.
    if let Some(TestInstr::SetParams(params)) = tf_read_instr(bytes, first) {
        params
    } else {
        HARNESS_PARAMETERS
    }
}

pub(crate) fn tf_read_instr(bytes: &[u8], instr: &TFInstr) -> Option<TestInstr> {
    const_assert!(TFInstr::COUNT == 8);
    match instr.kind {
        TFInstr::LOAD_POW => {
            let block = Block::zcash_deserialize(instr.data_slice(bytes)).ok()?;
            Some(TestInstr::LoadPoW(block))
        }

        TFInstr::LOAD_POS => {
            let block_and_fat_ptr =
                BftBlockAndFatPointerToItWrap::zcash_deserialize(instr.data_slice(bytes)).ok()?;
            Some(TestInstr::LoadPoS((
                block_and_fat_ptr.0.block,
                block_and_fat_ptr.0.fat_ptr,
            )))
        }

        TFInstr::SET_PARAMS => Some(TestInstr::SetParams(ZcashCrosslinkParameters {
            bc_confirmation_depth_sigma: instr.val[0],
            bootstrap: bootstrap_from_bytes(instr.data_slice(bytes))?,
        })),

        TFInstr::EXPECT_POW_CHAIN_LENGTH => {
            Some(TestInstr::ExpectPoWChainLength(instr.val[0] as u32))
        }
        TFInstr::EXPECT_POS_CHAIN_LENGTH => Some(TestInstr::ExpectPoSChainLength(instr.val[0])),

        TFInstr::EXPECT_POW_BLOCK_FINALITY => Some(TestInstr::ExpectPoWBlockFinality(
            ZebBlockHash(
                instr
                    .data_slice(bytes)
                    .try_into()
                    .expect("should be 32 bytes for hash"),
            ),
            finality_from_val(&instr.val),
        )),

        TFInstr::ROSTER_FORCE_INCLUDE => Some(TestInstr::RosterForceInclude(
            instr.data_slice(bytes).try_into().expect("32-byte array"),
            instr.val[0],
        )),
        TFInstr::EXPECT_ROSTER_INCLUDES => Some(TestInstr::ExpectRosterIncludes(
            instr.data_slice(bytes).try_into().expect("32-byte array"),
            instr.val[0],
        )),

        _ => {
            panic!("Unrecognized instruction {}", instr.kind);
            None
        }
    }
}

#[derive(Clone)]
pub(crate) enum TestInstr {
    LoadPoW(Block),
    LoadPoS((BftBlock, FatPointerToBftBlock)),
    SetParams(ZcashCrosslinkParameters),
    ExpectPoWChainLength(u32),
    ExpectPoSChainLength(u64),
    ExpectPoWBlockFinality(ZebBlockHash, Option<TFLBlockFinality>),
    RosterForceInclude([u8; 32], u64),   // public address
    ExpectRosterIncludes([u8; 32], u64), // public address
}

pub(crate) async fn handle_instr(
    internal_handle: &TFLServiceHandle,
    bytes: &[u8],
    instr: TestInstr,
    flags: u32,
    instr_i: usize,
) {
    match instr {
        TestInstr::LoadPoW(block) => {
            // let path = format!("../crosslink-test-data/test_pow_block_{}.bin", instr_i);
            // info!("writing binary at {}", path);
            // let mut file = std::fs::File::create(&path).expect("valid file");
            // file.write_all(instr.data_slice(bytes));

            // Route through new_network's ingest queue -- the same doorway submit_block uses --
            // so the tests exercise the production admission path rather than a parallel one.
            let (force_feed_ok, msg) = match zebra_state::new_network::submit_block_to_new_network(
                Arc::new(block),
                std::time::Duration::from_secs(30),
            ).await {
                Ok(zebra_state::new_network::IngestOutcome::Committed(_)) => (true, "PoW ingest ok".to_string()),
                Ok(zebra_state::new_network::IngestOutcome::Known { .. }) => (true, "PoW already known".to_string()),
                Ok(zebra_state::new_network::IngestOutcome::Failed { reason, .. }) => (false, reason),
                Err(msg) => (false, msg),
            };
            test_check(flags, force_feed_ok, &msg);
        }

        TestInstr::LoadPoS((block, fat_ptr)) => {
            // let path = format!("../crosslink-test-data/test_pos_block_{}.bin", instr_i);
            // info!("writing binary at {}", path);
            // let mut file = std::fs::File::create(&path).expect("valid file");
            // file.write_all(instr.data_slice(bytes)).expect("write success");

            let (force_feed_ok, msg) = match zebra_state::new_network::bft::force_feed_bft_block(Arc::new(block), fat_ptr).await {
                Ok(()) => (true, "PoS force feed ok".to_string()),
                Err(msg) => (false, msg),
            };
            test_check(flags, force_feed_ok, &msg);
        }

        TestInstr::SetParams(params) => {
            debug_assert!(instr_i == 0, "should only be set at the beginning");
            // Consensus parameters are fixed when the network is built, before the node boots (see
            // `crosslink_parameters_for_test`), so all that remains is confirming the node agrees.
            test_check(
                flags,
                params == internal_handle.params,
                &format!("SET_PARAMS: file declares {:?}, node runs {:?}", params, internal_handle.params),
            );
        }

        TestInstr::ExpectPoWChainLength(h) => {
            if let StateResponse::Tip(Some((height, hash))) =
                (internal_handle.call.state)(StateRequest::Tip)
                    .await
                    .expect("can read tip")
            {
                let expect = h;
                let actual = height.0 + 1;
                test_check(
                    flags,
                    expect == actual,
                    &format!("PoW chain length: expected {}, actually {}", expect, actual),
                ); // TODO: maybe assert in test but recoverable error in-GUI
            }
        }

        TestInstr::ExpectPoSChainLength(h) => {
            let expect = h as usize;
            let actual = zebra_state::new_network::bft::bft_chain().read().unwrap().blocks.len();
            test_check(
                flags,
                expect == actual,
                &format!("PoS chain length: expected {}, actually {}", expect, actual),
            ); // TODO: maybe assert in test but recoverable error in-GUI
        }

        TestInstr::ExpectPoWBlockFinality(hash, f) => {
            let expect = f;
            let height = block_height_from_hash(&internal_handle.call.clone(), hash).await;
            let actual = if let Some(height) = height {
                tfl_block_finality_from_height_hash(internal_handle.clone(), height, hash).await
            } else {
                Ok(None)
            }
            .expect("valid response, even if None");
            test_check(
                flags,
                expect == actual,
                &format!(
                    "PoW block finality at hash={}, height={:?}: expected {:?}, actually {:?}",
                    hash, height, expect, actual
                ),
            ); // TODO: maybe assert in test but recoverable error in-GUI
        }

        TestInstr::ExpectRosterIncludes(pub_key, stake) => {
            let key = PubKeyID(pub_key);
            let finalizer = zebra_state::new_network::bft::bft_chain()
                .read()
                .unwrap()
                .roster
                .iter()
                .find(|x| PubKeyID(x.pub_key) == key)
                .cloned();

            if let Some(finalizer) = finalizer {
                test_check(
                    flags,
                    stake == TEST_STAKE_IGNORED || stake == finalizer.voting_power,
                    &format!(
                        "Finalizer stake: expected {}, actually {}",
                        stake, finalizer.voting_power
                    ),
                );
            } else {
                test_check(
                    flags,
                    false,
                    &format!("Finalizer found: {:?}", key),
                );
            }
        }

        TestInstr::RosterForceInclude(pub_key, stake) => {
            zebra_state::new_network::bft::bft_chain()
                .write()
                .unwrap()
                .roster
                .push(RosterMember { pub_key, voting_power: stake, txids: Vec::new() });
        }
    }
}

pub async fn read_instrs(internal_handle: TFLServiceHandle, bytes: &[u8], instrs: &[TFInstr]) {
    // A failed deserialize is a hard error for a normal test but an expected input rejection
    // for the fuzzer; `uhh_option` decides which via TEST_ON_FAIL (PANIC vs recover).
    let on_fail = *TEST_ON_FAIL.lock().unwrap();
    for instr_i in 0..instrs.len() {
        // info!(
        //     "Loading instruction {}: {} ({})",
        //     instr_i,
        //     TFInstr::string_from_instr(bytes, &instrs[instr_i]),
        //     instrs[instr_i].kind
        // );

        if let Some(instr) = uhh_option(tf_read_instr(bytes, &instrs[instr_i]), on_fail) {
            handle_instr(
                &internal_handle,
                bytes,
                instr,
                instrs[instr_i].flags,
                instr_i,
            )
            .await;
        }

        *TEST_INSTR_C.lock().unwrap() = instr_i + 1; // accounts for end
    }
}

pub(crate) async fn instr_reader(internal_handle: TFLServiceHandle) {
    use zebra_chain::serialization::{ZcashDeserialize, ZcashSerialize};
    let call = internal_handle.call.clone();
    println!("waiting for tip before starting the test...");
    let before_time = Instant::now();
    loop {
        if let Ok(StateResponse::Tip(Some(_))) = (call.state)(StateRequest::Tip).await {
            break;
        } else {
            // warn!("Failed to read tip");
            if before_time.elapsed().as_secs() > 30 {
                panic!("Timeout waiting for test to start.");
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
    println!("Starting test!");

    if let Some(path) = TEST_INSTR_PATH.lock().unwrap().clone() {
        *TEST_INSTR_BYTES.lock().unwrap() = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) => panic!("Invalid test file: {:?}: {}", path, err), // TODO: specifics
        };
    }

    let bytes = TEST_INSTR_BYTES.lock().unwrap().clone();

    // Normal tests PANIC on an unparseable envelope; the fuzzer recovers (TEST_ON_FAIL).
    let on_fail = *TEST_ON_FAIL.lock().unwrap();
    let tf = match uhh(TF::read_from_bytes(&bytes), on_fail) {
        Ok(tf) => tf,
        Err(_) => return, // uhh already panicked (PANIC) or logged (fuzzer)
    };

    *TEST_INSTRS.lock().unwrap() = tf.instrs.clone();

    read_instrs(internal_handle, &bytes, &tf.instrs).await;

    // make sure tests completed
    assert_eq!(
        *TEST_INSTR_C.lock().unwrap(),
        tf.instrs.len(),
        "didn't complete test {}",
        TEST_NAME.lock().unwrap()
    );
    // make sure the test as a whole actually fails for failed instructions.
    // Include the recorded (instruction index, message) pairs in the message so a red test is
    // self-describing: otherwise these are collected but discarded here, and diagnosing which
    // instruction failed needs TEST_CHECK_ASSERT raised and a rebuild.
    //
    // The lock MUST be released before TEST_SHUTDOWN_FN below: the shutdown path
    // (crosslink shutdown fn -> dump_test_instrs) re-locks this same std Mutex on this thread,
    // and std Mutex is not reentrant, so holding the guard across the shutdown call deadlocks
    // every PASSING test at exit. (A failing test unwinds on the assert and drops the guard, so
    // only green tests hang.) Hence the explicit scope -- do not lift the binding out of it.
    {
        let failed_instrs = TEST_FAILED_INSTR_IDXS.lock().unwrap();
        assert!(
            failed_instrs.is_empty(),
            "failed test {}: {:?}",
            TEST_NAME.lock().unwrap(),
            *failed_instrs
        );
    }
    println!("Test done, shutting down");
    // #[cfg(feature = "viz_gui")]
    // tokio::time::sleep(Duration::from_secs(120)).await;

    TEST_SHUTDOWN_FN.lock().unwrap()();
}
