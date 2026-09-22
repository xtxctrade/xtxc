//! Economic receipts for opcode 13/14/18, bound to the exact prepared message.
//!
//! Admission is operator-pinned policy, never a request body from a wallet or
//! executor. Finalized RPC metadata is trusted only from the genesis-pinned RPC;
//! this is not a light-client proof. Amounts come from raw balances and the
//! settlement program's return data, never from displayed UI token amounts.
use crate::{
    feed::Snapshot,
    journal::{Entry, Phase},
    mesh::{FrozenIntent, ProductIdentity},
    onebook_wire::{
        instrument_id, issuer_id, stock_policy_v2_address, STOCK_POLICY_V2_LEN, STOCK_POLICY_V2_TAG,
    },
    pipeline,
    rpc::Rpc,
    Result,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use solana_pubkey::Pubkey;
use std::{collections::BTreeSet, str::FromStr};
use stocklana_adapters::{graph::Graph, u64_at};

const TOKEN: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const TOKEN_2022: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
const COMPUTE: &str = "ComputeBudget111111111111111111111111111111";
const SYSTEM: &str = "11111111111111111111111111111111";
const ASSOCIATED_TOKEN: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
const WSOL: &str = "So11111111111111111111111111111111111111112";
const DEFAULT_HEAP_FRAME_BYTES: u32 = 32 * 1024;
const MAXIMUM_HEAP_FRAME_BYTES: u32 = 256 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductAdmission {
    pub identity: ProductIdentity,
    pub policy: String,
    /// Hash of the exact onchain policy bytes in the admitted bank.
    pub policy_data_hash: [u8; 32],
    pub claim: Option<String>,
    pub claim_data_hash: Option<[u8; 32]>,
    pub policy_version: u64,
    pub model: u8,
    pub numerator: u64,
    pub denominator: u64,
    pub conservative_bps: u16,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Admission {
    pub settlement_program: String,
    /// Same trusted policy-set commitment used by the quote/freeze boundary.
    pub product_policy_hash: [u8; 32],
    pub maximum_cu: u64,
    pub maximum_heap_frame_bytes: u32,
    pub maximum_compute_price: u64,
    pub allow_underlying_closed: bool,
    pub products: Vec<ProductAdmission>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TokenBinding {
    pub account: String,
    pub mint: String,
    pub owner: String,
    pub token_program: String,
    pub decimals: u8,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProductBinding {
    token: TokenBinding,
    product_id: [u8; 32],
    issuer: String,
    model: u8,
    numerator: u64,
    denominator: u64,
    conservative_bps: u16,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SellerBinding {
    stock: TokenBinding,
    cash: TokenBinding,
    sequence: u64,
    stock_atoms: u64,
    cash_atoms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FundingBinding {
    /// Cash plus funding intermediates must end at their original balances.
    preserved: Vec<TokenBinding>,
    minimum_cash: u64,
    #[serde(default = "funding_version_one")]
    version: u8,
}

fn funding_version_one() -> u8 {
    1
}

/// Exact top-level wallet preparation bound into the approved message. Missing
/// accounts are admitted only as canonical system-owned empty addresses and
/// become observable token/nonce accounts inside the same atomic transaction.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SetupBinding {
    owner: String,
    created_tokens: Vec<TokenBinding>,
    created_nonce: Option<String>,
    wrap_token: Option<String>,
    wrap_lamports: u64,
    instruction_positions: Vec<u8>,
}

impl SetupBinding {
    fn created(&self, account: &str) -> bool {
        self.created_tokens
            .iter()
            .any(|token| token.account == account)
    }

    fn active(&self) -> bool {
        !self.created_tokens.is_empty() || self.created_nonce.is_some() || self.wrap_lamports != 0
    }

    /// Token parsing needs the state produced by the typed setup prefix. This
    /// projection is validation-only; its slot/hash/currentness remain those of
    /// the original bank and it is never described as confirmed chain state.
    fn projected(&self, snapshot: &Snapshot) -> Result<Snapshot> {
        let mut projected = snapshot.clone();
        for binding in &self.created_tokens {
            let account = projected
                .accounts
                .iter_mut()
                .find(|account| account.key == binding.account)
                .ok_or("created ATA absent from execution bank")?;
            if account.owner != SYSTEM || account.executable || !account.data.is_empty() {
                return Err("created ATA was not explicitly absent".into());
            }
            account.owner = binding.token_program.clone();
            account.data = vec![0; 165];
            account.data[..32].copy_from_slice(&key(&binding.mint)?.to_bytes());
            account.data[32..64].copy_from_slice(&key(&binding.owner)?.to_bytes());
            account.data[108] = 1;
            if binding.token_program == TOKEN_2022 {
                account.data.push(2);
            }
        }
        Ok(projected)
    }
}

/// Persist this alongside the approved wire in the trusted control plane. A
/// deserialized expectation must not be accepted from a public API caller.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedExposure {
    pub(crate) message_hash: [u8; 32],
    pub(crate) intent_commitment: [u8; 32],
    program: String,
    instrument: String,
    opcode: u8,
    pub(crate) prepared_slot: u64,
    deadline: u64,
    sequence: u64,
    input: u64,
    floor: u64,
    pub(crate) maximum_cu: u64,
    #[serde(default = "default_heap_frame_bytes")]
    heap_frame_bytes: u32,
    keys: Vec<String>,
    pub(crate) resources: Vec<[u8; 32]>,
    input_token: TokenBinding,
    products: Vec<ProductBinding>,
    sellers: Vec<SellerBinding>,
    #[serde(default)]
    funding: Option<FundingBinding>,
    #[serde(default)]
    direct_reflow: bool,
    #[serde(default)]
    setup: SetupBinding,
}

const fn default_heap_frame_bytes() -> u32 {
    DEFAULT_HEAP_FRAME_BYTES
}

impl ExpectedExposure {
    /// The funding token's owner is always bound to the signed intent. The
    /// optional setup prefix has an empty owner when all ATAs already exist.
    /// Using setup.owner hid those existing-wallet orders from reconciliation.
    pub(crate) fn wallet_owner(&self) -> &str { &self.input_token.owner }
}

pub struct VerifiedExposure {
    signature: String,
    instrument: String,
    input: u64,
    exposure: u64,
    products: Vec<Value>,
    cu: u64,
    fee: u64,
    slot: u64,
}

impl VerifiedExposure {
    pub fn summary(&self) -> Value {
        json!({"signature":self.signature,"instrument":self.instrument,
            "inputAtoms":self.input.to_string(),"actualExposureQ32":self.exposure.to_string(),
            "products":self.products,"computeUnits":self.cu,"feeLamports":self.fee,
            "slot":self.slot,"verification":"finalized_exact_wire_balances_and_sbf_exposure"})
    }
}

fn integer(d: &[u8], offset: usize) -> Result<u64> {
    u64_at(d, offset).map_err(|_| "exposure wire integer".into())
}

fn key(s: &str) -> Result<Pubkey> {
    Pubkey::from_str(s).map_err(|_| "exposure public key".into())
}

fn account<'a>(snapshot: &'a Snapshot, address: &str) -> Result<&'a crate::feed::Account> {
    snapshot
        .accounts
        .iter()
        .find(|a| a.key == address)
        .ok_or_else(|| "exposure account missing from bank".into())
}

fn token(
    snapshot: &Snapshot,
    address: &str,
    mint: &str,
    owner: &str,
    program: &str,
) -> Result<TokenBinding> {
    let mint_account = account(snapshot, mint)?;
    if ![TOKEN, TOKEN_2022].contains(&program)
        || mint_account.owner != program
        || mint_account.executable
        || mint_account.data.len() < 82
        || mint_account.data[45] != 1
    {
        return Err("exposure token program/mint".into());
    }
    let binding = TokenBinding {
        account: address.into(),
        mint: mint.into(),
        owner: owner.into(),
        token_program: program.into(),
        decimals: mint_account.data[44],
    };
    snapshot_amount(snapshot, &binding)?;
    Ok(binding)
}

fn explicit_empty(snapshot: &Snapshot, address: &str) -> Result<()> {
    let value = account(snapshot, address)?;
    if value.owner != SYSTEM || value.executable || !value.data.is_empty() {
        return Err("wallet setup account is not explicitly absent".into());
    }
    Ok(())
}

fn parse_setup(
    snapshot: &Snapshot,
    msg: &solana_message::VersionedMessage,
    keys: &[String],
    intent: &FrozenIntent,
    admission: &Admission,
) -> Result<(SetupBinding, BTreeSet<usize>)> {
    let owner = key(&intent.owner)?;
    let settlement = key(&admission.settlement_program)?;
    let associated = key(ASSOCIATED_TOKEN)?;
    let token = key(TOKEN)?;
    let wsol = key(WSOL)?;
    let required = usize::from(msg.header().num_required_signatures);
    let at = |index: u8| {
        keys.get(usize::from(index))
            .map(String::as_str)
            .ok_or_else(|| "wallet setup account index".to_string())
    };
    let is_signer = |index: u8| usize::from(index) < required;
    let writable = |index: u8| msg.is_maybe_writable(usize::from(index), None);
    let mut setup = SetupBinding::default();
    let mut positions = BTreeSet::new();
    let mut phase = 0u8; // 0 before setup, 1 creates, 2 transfer awaits sync, 3 synced.
    let mut seen_nonce = false;
    for (position, ix) in msg.instructions().iter().enumerate() {
        let program = at(ix.program_id_index)?;
        if program == COMPUTE {
            if phase != 0 {
                return Err("compute instruction after wallet setup".into());
            }
            continue;
        }
        if program == admission.settlement_program
            && !(ix.data == [0] && position + 1 != msg.instructions().len())
        {
            if position + 1 != msg.instructions().len() || phase == 2 {
                return Err("wallet setup order/final settlement".into());
            }
            break;
        }
        match program {
            p if p == admission.settlement_program && ix.data == [0] => {
                if phase > 1 || seen_nonce || ix.accounts.len() != 3 {
                    return Err("wallet nonce initialization shape".into());
                }
                let [payer_i, nonce_i, system_i]: [u8; 3] = ix
                    .accounts
                    .as_slice()
                    .try_into()
                    .map_err(|_| "wallet nonce accounts")?;
                let nonce =
                    Pubkey::find_program_address(&[b"stocklana", owner.as_ref()], &settlement).0;
                if at(payer_i)? != intent.owner
                    || at(nonce_i)? != nonce.to_string()
                    || at(system_i)? != SYSTEM
                    || !is_signer(payer_i)
                    || !writable(payer_i)
                    || !writable(nonce_i)
                    || writable(system_i)
                {
                    return Err("wallet nonce initialization binding".into());
                }
                explicit_empty(snapshot, at(nonce_i)?)?;
                let program_account = account(snapshot, &admission.settlement_program)?;
                if !program_account.executable {
                    return Err("wallet setup settlement is not executable".into());
                }
                setup.created_nonce = Some(nonce.to_string());
                seen_nonce = true;
                phase = 1;
                positions.insert(position);
            }
            ASSOCIATED_TOKEN => {
                if phase > 1 || ix.data != [1] || ix.accounts.len() != 6 {
                    return Err("wallet ATA create shape/order".into());
                }
                let [payer_i, destination_i, owner_i, mint_i, system_i, program_i]: [u8; 6] = ix
                    .accounts
                    .as_slice()
                    .try_into()
                    .map_err(|_| "wallet ATA accounts")?;
                let mint = key(at(mint_i)?)?;
                let token_program = key(at(program_i)?)?;
                if ![token, key(TOKEN_2022)?].contains(&token_program)
                    || at(payer_i)? != intent.owner
                    || at(owner_i)? != intent.owner
                    || at(system_i)? != SYSTEM
                    || !is_signer(payer_i)
                    || !writable(payer_i)
                    || !writable(destination_i)
                    || writable(mint_i)
                    || writable(system_i)
                    || writable(program_i)
                {
                    return Err("wallet ATA create privileges".into());
                }
                let destination = Pubkey::find_program_address(
                    &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
                    &associated,
                )
                .0;
                if at(destination_i)? != destination.to_string()
                    || setup
                        .created_tokens
                        .iter()
                        .any(|binding| binding.account == destination.to_string())
                {
                    return Err("wallet ATA canonical address/duplicate".into());
                }
                explicit_empty(snapshot, at(destination_i)?)?;
                let mint_account = account(snapshot, at(mint_i)?)?;
                let ata_program = account(snapshot, ASSOCIATED_TOKEN)?;
                let token_program_account = account(snapshot, at(program_i)?)?;
                if mint_account.owner != at(program_i)?
                    || mint_account.executable
                    || mint_account.data.len() < 82
                    || mint_account.data[45] != 1
                    || !ata_program.executable
                    || !token_program_account.executable
                {
                    return Err("wallet ATA program/mint binding".into());
                }
                setup.created_tokens.push(TokenBinding {
                    account: destination.to_string(),
                    mint: mint.to_string(),
                    owner: owner.to_string(),
                    token_program: token_program.to_string(),
                    decimals: mint_account.data[44],
                });
                phase = 1;
                positions.insert(position);
            }
            SYSTEM => {
                if phase > 1 || setup.wrap_lamports != 0 || ix.accounts.len() != 2 {
                    return Err("wallet native transfer shape/order".into());
                }
                let [owner_i, destination_i]: [u8; 2] = ix
                    .accounts
                    .as_slice()
                    .try_into()
                    .map_err(|_| "wallet native transfer accounts")?;
                if ix.data.len() != 12
                    || u32::from_le_bytes(ix.data[..4].try_into().unwrap()) != 2
                    || at(owner_i)? != intent.owner
                    || !is_signer(owner_i)
                    || !writable(owner_i)
                    || !writable(destination_i)
                {
                    return Err("wallet native transfer binding".into());
                }
                let lamports = integer(&ix.data, 4)?;
                let destination = Pubkey::find_program_address(
                    &[owner.as_ref(), token.as_ref(), wsol.as_ref()],
                    &associated,
                )
                .0;
                if lamports == 0
                    || lamports > stocklana_adapters::MAX_INPUT
                    || at(destination_i)? != destination.to_string()
                {
                    return Err("wallet native transfer amount/address".into());
                }
                if !setup.created(destination.to_string().as_str()) {
                    let binding = TokenBinding {
                        account: destination.to_string(),
                        mint: WSOL.into(),
                        owner: intent.owner.clone(),
                        token_program: TOKEN.into(),
                        decimals: 9,
                    };
                    raw_amount(account(snapshot, at(destination_i)?)?, &binding)?;
                }
                setup.wrap_lamports = lamports;
                setup.wrap_token = Some(destination.to_string());
                phase = 2;
                positions.insert(position);
            }
            TOKEN => {
                if phase != 2 || ix.data != [17] || ix.accounts.len() != 1 {
                    return Err("wallet SyncNative shape/order".into());
                }
                let destination_i = ix.accounts[0];
                if setup.wrap_token.as_deref() != Some(at(destination_i)?)
                    || !writable(destination_i)
                {
                    return Err("wallet SyncNative binding".into());
                }
                phase = 3;
                positions.insert(position);
            }
            _ => return Err("unexpected wallet setup action".into()),
        }
    }
    if positions.len() > crate::wallet_wire::MAX_WALLET_SETUP_INSTRUCTIONS
        || setup.created_tokens.len() > crate::wallet_wire::MAX_WALLET_ASSETS
    {
        return Err("wallet setup action count".into());
    }
    if phase == 2 {
        return Err("native transfer missing SyncNative".into());
    }
    if setup.active() {
        setup.owner = intent.owner.clone();
        setup.instruction_positions = positions
            .iter()
            .map(|position| u8::try_from(*position).map_err(|_| "wallet setup position".into()))
            .collect::<Result<Vec<_>>>()?;
    }
    Ok((setup, positions))
}

pub(crate) fn raw_amount(value: &crate::feed::Account, binding: &TokenBinding) -> Result<u64> {
    if value.key != binding.account
        || value.owner != binding.token_program
        || value.executable
        || value.data.len() < 165
        || value.data[..32] != key(&binding.mint)?.to_bytes()
        || value.data[32..64] != key(&binding.owner)?.to_bytes()
        || value.data[108] != 1
        || value.data[72..76] != [0; 4]
        || value.data[121..129] != [0; 8]
        || value.data[129..133] != [0; 4]
    {
        return Err("exposure token balance identity".into());
    }
    integer(&value.data, 64)
}

pub(crate) fn snapshot_amount(snapshot: &Snapshot, binding: &TokenBinding) -> Result<u64> {
    raw_amount(account(snapshot, &binding.account)?, binding)
}

impl ExpectedExposure {
    pub(crate) fn heap_frame_bytes(&self) -> u32 {
        self.heap_frame_bytes
    }

    /// Decode the actual v0/legacy message and bind every product, seller,
    /// policy, fee and balance account to the frozen economic intent.
    pub fn bind(
        snapshot: &Snapshot,
        intent: &FrozenIntent,
        admission: &Admission,
        message: &[u8],
    ) -> Result<Self> {
        Self::bind_market(
            snapshot,
            &snapshot
                .accounts
                .iter()
                .map(|a| a.key.clone())
                .collect::<Vec<_>>(),
            intent,
            admission,
            message,
        )
    }

    /// `market_keys` comes from the operator's admitted market layout. Wallet,
    /// nonce, policy and ALT accounts are validated in the full execution bank;
    /// the original economic intent still commits to the exact market subset.
    pub fn bind_market(
        snapshot: &Snapshot,
        market_keys: &[String],
        intent: &FrozenIntent,
        admission: &Admission,
        message: &[u8],
    ) -> Result<Self> {
        let commitment = intent.commitment(snapshot.slot)?;
        if snapshot.project(market_keys)?.hash != intent.world_generation_hash
            || admission.product_policy_hash != intent.product_policy_hash
            || admission.maximum_cu == 0
            || admission.maximum_cu > 1_400_000
            || !(DEFAULT_HEAP_FRAME_BYTES..=MAXIMUM_HEAP_FRAME_BYTES)
                .contains(&admission.maximum_heap_frame_bytes)
            || !admission.maximum_heap_frame_bytes.is_multiple_of(1024)
            || admission.products.is_empty()
            || admission.products.len() > 8
        {
            return Err("exposure admission commitment".into());
        }
        let msg = pipeline::decode(message)?;
        let keys = pipeline::resolved(&msg, snapshot)?;
        let n = usize::from(msg.header().num_required_signatures);
        if !(1..=4).contains(&n)
            || message.len() + 1 + 64 * n > 1232
            || msg.recent_blockhash().to_bytes() == [0; 32]
            || msg.instructions().len() > crate::wallet_wire::MAX_STOCK_TRANSACTION_INSTRUCTIONS
            || !msg.static_account_keys()[..n]
                .iter()
                .any(|k| k.to_string() == intent.owner)
        {
            return Err("exposure message/signature bounds".into());
        }
        let (setup, setup_positions) = parse_setup(snapshot, &msg, &keys, intent, admission)?;
        if setup.active()
            && msg.static_account_keys().first().map(ToString::to_string)
                != Some(intent.owner.clone())
        {
            return Err("wallet setup owner must pay transaction fee".into());
        }
        let wallet_snapshot = setup.projected(snapshot)?;
        let mut settlement = None;
        let mut cu = None;
        let mut heap_frame = None;
        let mut price = None;
        for (position, ix) in msg.instructions().iter().enumerate() {
            if setup_positions.contains(&position) {
                continue;
            }
            let program = keys
                .get(usize::from(ix.program_id_index))
                .ok_or("program index")?;
            if program == &admission.settlement_program {
                if settlement.replace(ix).is_some() || position + 1 != msg.instructions().len() {
                    return Err("exposure settlement must be unique and final".into());
                }
            } else if program == COMPUTE && ix.accounts.is_empty() {
                match ix.data.first() {
                    Some(2) if ix.data.len() == 5 && cu.is_none() => {
                        cu = Some(u64::from(u32::from_le_bytes(
                            ix.data[1..5].try_into().unwrap(),
                        )))
                    }
                    Some(1) if ix.data.len() == 5 && heap_frame.is_none() => {
                        heap_frame = Some(u32::from_le_bytes(ix.data[1..5].try_into().unwrap()))
                    }
                    Some(3) if ix.data.len() == 9 && price.is_none() => {
                        price = Some(integer(&ix.data, 1)?)
                    }
                    _ => return Err("exposure compute instruction".into()),
                }
            } else {
                return Err("unexpected exposure top-level action".into());
            }
        }
        let maximum_cu = cu
            .filter(|c| *c > 0 && *c <= admission.maximum_cu)
            .ok_or("exposure compute ceiling")?;
        let heap_frame_bytes = heap_frame.unwrap_or(DEFAULT_HEAP_FRAME_BYTES);
        if !(DEFAULT_HEAP_FRAME_BYTES..=admission.maximum_heap_frame_bytes)
            .contains(&heap_frame_bytes)
            || heap_frame_bytes % 1024 != 0
        {
            return Err("exposure heap frame".into());
        }
        if price.unwrap_or(0) > admission.maximum_compute_price {
            return Err("exposure priority fee".into());
        }
        let ix = settlement.ok_or("exposure settlement missing")?;
        let local = ix
            .accounts
            .iter()
            .map(|i| {
                keys.get(usize::from(*i))
                    .cloned()
                    .ok_or_else(|| "exposure account index".into())
            })
            .collect::<Result<Vec<_>>>()?;
        if local.iter().collect::<BTreeSet<_>>().len() != local.len() {
            return Err("exposure duplicate local account".into());
        }
        let at = |index: u8| {
            local
                .get(usize::from(index))
                .map(String::as_str)
                .ok_or_else(|| "exposure local index".to_string())
        };
        let mut d = ix.data.as_slice();
        let funding_graph = if d.first() == Some(&18) {
            if !(8..=1024).contains(&d.len())
                || !matches!(d[1], 1 | 2)
                || (d[1] == 2 && d[2] != 0)
                || d[3] != 0
            {
                return Err("funded exposure envelope".into());
            }
            let version = d[1];
            let len = usize::from(u16::from_le_bytes([d[4], d[5]]));
            let cell_len = usize::from(u16::from_le_bytes([d[6], d[7]]));
            let end = 8usize.checked_add(len).ok_or("funding graph length")?;
            if end.checked_add(cell_len) != Some(d.len()) {
                return Err("funded exposure length".into());
            }
            let graph = Graph::decode(d.get(8..end).ok_or("funding bytes")?, local.len())
                .map_err(|_| "funding graph")?;
            let selector = usize::from(d[2]);
            if d[8] != 2 || graph.leg_count > 3 {
                return Err("funding fixed graph bound".into());
            }
            d = d.get(end..).ok_or("funded cell")?;
            if d.first() != Some(&14)
                || d.len() < 56
                || (version == 1 && selector >= usize::from(d[1]))
                || (version == 2 && d[3] != 1)
            {
                return Err("funded product selection".into());
            }
            Some((graph, selector, version))
        } else {
            None
        };
        let (mut opcode, count, deadline, floor, age, allow_closed) = match d.first() {
            Some(13) if d.len() >= 28 && d[3] == 0 => (
                13,
                d[1] as usize,
                integer(d, 4)?,
                integer(d, 12)?,
                integer(d, 20)?,
                d[2],
            ),
            Some(14) if d.len() >= 56 && d[5] <= 1 && d[6..8] == [0; 2] && d[53..56] == [0; 3] => (
                14,
                d[1] as usize,
                integer(d, 8)?,
                integer(d, 40)?,
                integer(d, 16)?,
                d[4],
            ),
            _ => return Err("economic exposure opcode required".into()),
        };
        if funding_graph.is_some() {
            opcode = 18;
        }
        let direct_reflow = opcode == 14 && d[5] == 1;
        if !(1..=4).contains(&count)
            || deadline < snapshot.slot
            || deadline > intent.deadline_slot
            || u128::from(floor) < intent.minimum_exposure_q32
            || !(1..=150).contains(&age)
            || allow_closed > u8::from(admission.allow_underlying_closed)
        {
            return Err("signed exposure floor/session/deadline".into());
        }

        let mut products = Vec::new();
        let mut sellers = Vec::new();
        let mut used = BTreeSet::new();
        let policy_cash_mint = if opcode == 18 {
            at(d[51])?
        } else {
            &intent.input_mint
        };
        // Both opcodes use the same conversion fields but different wire offsets.
        let mut product = |policy: u8,
                           claim: Option<u8>,
                           destination: u8,
                           mint: u8,
                           program: u8,
                           model: u8,
                           bps: u16,
                           version: u64,
                           num: u64,
                           den: u64|
         -> Result<()> {
            let matches = admission
                .products
                .iter()
                .filter(|p| p.identity.mint == at(mint).unwrap_or(""))
                .collect::<Vec<_>>();
            if matches.len() != 1 {
                return Err("issuer product not uniquely admitted".into());
            }
            let p = matches[0];
            let id = p.identity.id()?;
            if p.identity.instrument != intent.instrument
                || !intent.admitted_product_ids.contains(&id)
                || !used.insert(id)
                || p.policy != at(policy)?
                || p.identity.token_program != at(program)?
                || p.policy_version != version
                || p.model != model
                || p.conservative_bps != bps
                || p.numerator != num
                || p.denominator != den
                || !matches!(model, 0 | 1)
                || num == 0
                || den == 0
                || !(1..=10000).contains(&bps)
            {
                return Err("signed issuer/conversion substitution".into());
            }
            let state = account(snapshot, &p.policy)?;
            let policy_authority = state
                .data
                .get(8..40)
                .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
                .map(Pubkey::new_from_array)
                .ok_or("ProductPolicy v2 authority")?;
            let policy_instrument = instrument_id(&p.identity.instrument)?;
            let policy_issuer = issuer_id(&p.identity.issuer)?;
            let product_mint = key(&p.identity.mint)?;
            let expected_policy = stock_policy_v2_address(
                &key(&admission.settlement_program)?,
                &policy_authority,
                &policy_instrument,
                &key(policy_cash_mint)?,
                &product_mint,
                &p.identity.rights_hash,
            );
            if state.owner != admission.settlement_program
                || state.executable
                || state.data.len() != STOCK_POLICY_V2_LEN
                || &state.data[..8] != STOCK_POLICY_V2_TAG
                || <[u8; 32]>::from(Sha256::digest(&state.data)) != p.policy_data_hash
                || p.policy != expected_policy.to_string()
                || state.data[40..72] != policy_instrument
                || state.data[72..104] != policy_issuer
                || state.data[104..136] != key(policy_cash_mint)?.to_bytes()
                || state.data[136..168] != product_mint.to_bytes()
                || state.data[168..200] != p.identity.rights_hash
                || integer(&state.data, 200)? != version
                || integer(&state.data, 208)? > snapshot.slot
                || snapshot.slot - integer(&state.data, 208)? > age
                || integer(&state.data, 216)? < deadline
                || state.data[224] & 1 == 0
                || state.data[224] & 8 != 0
                || (allow_closed == 0 && state.data[224] & 16 == 0)
            {
                return Err("onchain exposure policy missing/stale/halted".into());
            }
            let writable = |address: &str| {
                keys.iter()
                    .position(|k| k == address)
                    .is_some_and(|i| msg.is_maybe_writable(i, None))
            };
            if writable(&p.policy) || writable(&p.identity.mint) || !writable(at(destination)?) {
                return Err("exposure product account privileges".into());
            }
            if let Some(index) = claim {
                let address = at(index)?;
                let state = account(snapshot, address)?;
                if p.claim.as_deref() != Some(address)
                    || p.claim_data_hash != Some(Sha256::digest(&state.data).into())
                    || state.owner != admission.settlement_program
                    || state.executable
                    || writable(address)
                    || state.data.len() != 288
                    || &state.data[..8] != b"SKEWCLM1"
                {
                    return Err("ProductClaim binding".into());
                }
            }
            let binding = token(
                &wallet_snapshot,
                at(destination)?,
                at(mint)?,
                &intent.owner,
                at(program)?,
            )?;
            if binding.decimals != p.identity.raw_decimals {
                return Err("product decimals changed".into());
            }
            products.push(ProductBinding {
                token: binding,
                product_id: id,
                issuer: p.identity.issuer.clone(),
                model,
                numerator: num,
                denominator: den,
                conservative_bps: bps,
            });
            Ok(())
        };
        let (mut input_token, mut input, sequence, nonce) = if opcode == 13 {
            if local.first() != Some(&intent.owner) {
                return Err("exposure buyer identity".into());
            }
            let mut cursor = 28;
            let mut source = None;
            let mut sequence = None;
            let mut input = 0u64;
            for _ in 0..count {
                let row = d.get(cursor..cursor + 32).ok_or("exposure descriptor")?;
                let len = usize::from(u16::from_le_bytes([row[28], row[29]]));
                cursor += 32;
                let bytes = d.get(cursor..cursor + len).ok_or("exposure graph bytes")?;
                cursor += len;
                if row[30..32] != [0; 2] {
                    return Err("exposure descriptor reserved bytes".into());
                }
                let graph =
                    Graph::decode(bytes, local.len()).map_err(|_| "exposure graph decode")?;
                let src = graph.assets[0];
                let dst = graph.assets[graph.asset_count - 1];
                let current = (src.token, src.mint, src.program);
                if source.is_some_and(|s| s != current)
                    || sequence.is_some_and(|s| s != graph.sequence)
                    || graph.deadline > deadline
                    || graph.deadline < snapshot.slot
                {
                    return Err("exposure graph source/sequence/deadline".into());
                }
                source = Some(current);
                sequence = Some(graph.sequence);
                input = input
                    .checked_add(graph.input)
                    .ok_or("exposure input overflow")?;
                product(
                    row[0],
                    None,
                    dst.token,
                    dst.mint,
                    dst.program,
                    row[1],
                    u16::from_le_bytes([row[2], row[3]]),
                    integer(row, 4)?,
                    integer(row, 12)?,
                    integer(row, 20)?,
                )?;
            }
            if cursor != d.len() {
                return Err("exposure graph trailing bytes".into());
            }
            let (a, m, p) = source.ok_or("exposure source")?;
            (
                token(&wallet_snapshot, at(a)?, at(m)?, &intent.owner, at(p)?)?,
                input,
                sequence.ok_or("exposure sequence")?,
                at(1)?,
            )
        } else {
            if at(d[48])? != intent.owner
                || d[2] > 3
                || (d[2] == 0 && opcode != 18 && !direct_reflow)
                || usize::from(d[3]) > count
            {
                return Err("MeshFill buyer/seller bounds".into());
            }
            for i in 0..count {
                let row = d
                    .get(56 + i * 32..56 + (i + 1) * 32)
                    .ok_or("MeshFill product row")?;
                product(
                    row[0],
                    if row[5] == 0 { None } else { Some(row[5]) },
                    row[1],
                    row[2],
                    row[3],
                    row[4],
                    u16::from_le_bytes([row[6], row[7]]),
                    integer(row, 8)?,
                    integer(row, 16)?,
                    integer(row, 24)?,
                )?;
            }
            (
                token(
                    &wallet_snapshot,
                    at(d[50])?,
                    at(d[51])?,
                    &intent.owner,
                    at(d[52])?,
                )?,
                integer(d, 32)?,
                integer(d, 24)?,
                at(d[49])?,
            )
        };
        let execution_source = input_token.clone();
        let execution_input = input;
        let funding = if let Some((graph, _, version)) = &funding_graph {
            let source = graph.assets[0];
            let sink = graph.assets[graph.asset_count - 1];
            if graph.sequence != sequence
                || graph.deadline > deadline
                || graph.deadline < snapshot.slot
                || graph.min_out < execution_input
                || at(sink.token)? != execution_source.account
                || at(sink.mint)? != execution_source.mint
                || at(sink.program)? != execution_source.token_program
                || source.token == sink.token
                || source.mint == sink.mint
            {
                return Err("funding source/cash/sequence binding".into());
            }
            input_token = token(
                &wallet_snapshot,
                at(source.token)?,
                at(source.mint)?,
                &intent.owner,
                at(source.program)?,
            )?;
            input = graph.input;
            let preserved = graph.assets[1..graph.asset_count]
                .iter()
                .map(|asset| {
                    token(
                        &wallet_snapshot,
                        at(asset.token)?,
                        at(asset.mint)?,
                        &intent.owner,
                        at(asset.program)?,
                    )
                })
                .collect::<Result<Vec<_>>>()?;
            Some(FundingBinding {
                preserved,
                minimum_cash: graph.min_out,
                version: *version,
            })
        } else {
            None
        };
        if input != intent.input_atoms
            || input_token.mint != intent.input_mint
            || sequence != intent.owner_nonce
        {
            return Err("signed exposure input/nonce differs from intent".into());
        }
        if setup.wrap_lamports != 0
            && (setup.wrap_lamports != input
                || input_token.mint != WSOL
                || setup.wrap_token.as_deref() != Some(input_token.account.as_str()))
        {
            return Err("native wrapping differs from economic input".into());
        }
        if let Some(created_nonce) = &setup.created_nonce {
            if created_nonce != nonce || sequence != 0 {
                return Err("initialized nonce differs from settlement sequence".into());
            }
        }
        if setup
            .created_tokens
            .iter()
            .any(|binding| !local.contains(&binding.account))
        {
            return Err("created ATA is unused by settlement".into());
        }
        let nonce_account = account(snapshot, nonce)?;
        let expected_nonce = Pubkey::find_program_address(
            &[b"stocklana", key(&intent.owner)?.as_ref()],
            &key(&admission.settlement_program)?,
        )
        .0;
        let initialized_here = setup.created_nonce.as_deref() == Some(nonce);
        if key(nonce)? != expected_nonce
            || if initialized_here {
                nonce_account.owner != SYSTEM
                    || nonce_account.executable
                    || !nonce_account.data.is_empty()
                    || sequence != 0
            } else {
                nonce_account.owner != admission.settlement_program
                    || nonce_account.executable
                    || nonce_account.data.len() != 64
                    || &nonce_account.data[..8] != b"SKEWSEQ1"
                    || nonce_account.data[8..40] != key(&intent.owner)?.to_bytes()
                    || integer(&nonce_account.data, 40)? != sequence
            }
        {
            return Err("exposure owner nonce binding".into());
        }
        if opcode != 13 {
            let mut cursor = 56 + count * 32;
            let mut debit = 0u64;
            for _ in 0..d[2] {
                let row = d.get(cursor..cursor + 40).ok_or("MeshFill seller row")?;
                cursor += 40;
                let p = products
                    .get(usize::from(row[0]))
                    .ok_or("MeshFill seller product")?;
                if row[5] > 1
                    || row[6..8] != [0; 2]
                    || integer(row, 32)? == 0
                    || integer(row, 24)? < integer(row, 32)?
                {
                    return Err("MeshFill seller bounds".into());
                }
                let owner = at(row[1])?;
                let stock_owner = if row[5] == 1 { at(row[2])? } else { owner };
                sellers.push(SellerBinding {
                    stock: token(
                        snapshot,
                        at(row[3])?,
                        &p.token.mint,
                        stock_owner,
                        &p.token.token_program,
                    )?,
                    cash: token(
                        snapshot,
                        at(row[4])?,
                        &execution_source.mint,
                        owner,
                        &execution_source.token_program,
                    )?,
                    sequence: integer(row, 8)?,
                    stock_atoms: integer(row, 16)?,
                    cash_atoms: integer(row, 24)?,
                });
                debit = debit
                    .checked_add(integer(row, 24)?)
                    .ok_or("MeshFill internal cash overflow")?;
            }
            let mut residual_products = BTreeSet::new();
            let mut maximum_legs = funding_graph
                .as_ref()
                .map_or(0, |(graph, _, _)| graph.leg_count);
            let mut has_surplus = false;
            let funding_version = funding_graph.as_ref().map_or(0, |(_, _, version)| *version);
            let global_reflow = funding_version == 2 || direct_reflow;
            for _ in 0..d[3] {
                let row = d.get(cursor..cursor + 4).ok_or("MeshFill residual row")?;
                cursor += 4;
                if row[3] != 0
                    || (!global_reflow && !residual_products.insert(row[0]))
                    || (global_reflow && row[0] != u8::MAX)
                {
                    return Err("MeshFill duplicate residual".into());
                }
                let len = usize::from(u16::from_le_bytes([row[1], row[2]]));
                let graph = Graph::decode(
                    d.get(cursor..cursor + len).ok_or("MeshFill graph bytes")?,
                    local.len(),
                )
                .map_err(|_| "MeshFill graph decode")?;
                cursor += len;
                let src = graph.assets[0];
                if graph.sequence != sequence
                    || graph.deadline > deadline
                    || graph.deadline < snapshot.slot
                    || at(src.token)? != execution_source.account
                    || at(src.mint)? != execution_source.mint
                    || at(src.program)? != execution_source.token_program
                {
                    return Err("MeshFill residual identity".into());
                }
                if global_reflow {
                    if graph.reflow_calls == 0
                        || !graph.legs[..graph.leg_count].iter().flatten().all(|leg| {
                            leg.destination > 0
                                && (leg.source == 0
                                    || graph.legs[..graph.leg_count].iter().flatten().any(|head| {
                                        head.source == 0 && head.destination == leg.source
                                    }))
                        })
                        || !(1..graph.asset_count).all(|asset| {
                            graph.legs[..graph.leg_count]
                                .iter()
                                .flatten()
                                .any(|leg| leg.destination == asset)
                        })
                        || maximum_legs >= 4
                    {
                        return Err("economic global reflow bound".into());
                    }
                    let mut outputs = BTreeSet::new();
                    for asset in &graph.assets[1..graph.asset_count] {
                        let matches = products
                            .iter()
                            .filter(|product| {
                                at(asset.token).ok() == Some(product.token.account.as_str())
                                    && at(asset.mint).ok() == Some(product.token.mint.as_str())
                                    && at(asset.program).ok()
                                        == Some(product.token.token_program.as_str())
                            })
                            .count();
                        if matches != 1 || !outputs.insert(asset.token) {
                            return Err("economic global product binding".into());
                        }
                    }
                } else {
                    let p = products
                        .get(usize::from(row[0]))
                        .ok_or("MeshFill residual product")?;
                    let dst = graph.assets[graph.asset_count - 1];
                    if at(dst.token)? != p.token.account
                        || at(dst.mint)? != p.token.mint
                        || at(dst.program)? != p.token.token_program
                    {
                        return Err("MeshFill residual product identity".into());
                    }
                    maximum_legs += if graph.reflow_calls > 0 {
                        4
                    } else {
                        graph.leg_count
                    };
                    if maximum_legs > 4 {
                        return Err("economic total external leg bound".into());
                    }
                    if let Some((_, product, version)) = &funding_graph {
                        if *version == 1 && usize::from(row[0]) == *product {
                            if graph.reflow_calls != 0
                                || graph.legs[..graph.leg_count]
                                    .iter()
                                    .flatten()
                                    .rfind(|leg| leg.source == 0)
                                    .is_none_or(|leg| leg.budget != u64::MAX)
                            {
                                return Err("funding surplus consumer".into());
                            }
                            has_surplus = true;
                        }
                    }
                }
                debit = debit
                    .checked_add(graph.input)
                    .ok_or("MeshFill residual overflow")?;
            }
            if cursor != d.len()
                || debit != execution_input
                || (opcode == 18 && funding_version == 1 && !has_surplus)
                || (global_reflow && d[3] != 1)
            {
                return Err("MeshFill cash conservation".into());
            }
        }
        let resources = keys
            .iter()
            .enumerate()
            .filter(|(i, _)| msg.is_maybe_writable(*i, None))
            .map(|(_, k)| key(k).map(|k| k.to_bytes()))
            .collect::<Result<Vec<_>>>()?;
        let expected = Self {
            message_hash: Sha256::digest(message).into(),
            intent_commitment: commitment,
            program: admission.settlement_program.clone(),
            instrument: intent.instrument.clone(),
            opcode,
            prepared_slot: snapshot.slot,
            deadline,
            sequence,
            input,
            floor,
            maximum_cu,
            heap_frame_bytes,
            keys,
            resources,
            input_token,
            products,
            sellers,
            funding,
            direct_reflow,
            setup,
        };
        let balances = expected.balance_bindings();
        // Extra ATA creates must not turn a larger first-use envelope into an
        // unrelated rent-spend primitive. Every created token belongs to the
        // admitted trade, including its funding intermediate.
        if expected.setup.created_tokens.iter().any(|created| {
            !balances.iter().any(|binding| binding.account == created.account)
        }) {
            return Err("wallet setup asset is not used by the trade".into());
        }
        if balances
            .iter()
            .map(|b| &b.account)
            .collect::<BTreeSet<_>>()
            .len()
            != balances.len()
        {
            return Err("exposure balance account alias".into());
        }
        Ok(expected)
    }

    pub(crate) fn balance_bindings(&self) -> Vec<&TokenBinding> {
        let mut out = vec![&self.input_token];
        out.extend(self.products.iter().map(|p| &p.token));
        for seller in &self.sellers {
            out.extend([&seller.stock, &seller.cash]);
        }
        if let Some(funding) = &self.funding {
            out.extend(funding.preserved.iter());
        }
        out
    }

    pub(crate) fn observation_bindings(&self) -> Vec<&TokenBinding> {
        let mut out = self.balance_bindings();
        for token in &self.setup.created_tokens {
            if !out.iter().any(|binding| binding.account == token.account) {
                out.push(token);
            }
        }
        out
    }

    pub(crate) fn before_amount(&self, snapshot: &Snapshot, binding: &TokenBinding) -> Result<u64> {
        if self.setup.created(&binding.account) {
            explicit_empty(snapshot, &binding.account)?;
            Ok(0)
        } else {
            snapshot_amount(snapshot, binding)
        }
    }

    pub(crate) fn setup_observation_addresses(&self) -> Vec<String> {
        if !self.setup.active() {
            return Vec::new();
        }
        let mut out = vec![self.setup.owner.clone()];
        if let Some(nonce) = &self.setup.created_nonce {
            out.push(nonce.clone());
        }
        out
    }

    /// Produce the exact setup prefix from the already-approved message. The
    /// result is used only to observe initialized wallet accounts for lowering;
    /// the complete original message is still the sole settlement candidate.
    pub(crate) fn setup_message(&self, message: &[u8]) -> Result<Option<Vec<u8>>> {
        if !self.setup.active() {
            return Ok(None);
        }
        if <[u8; 32]>::from(Sha256::digest(message)) != self.message_hash {
            return Err("wallet setup message hash".into());
        }
        let mut msg = pipeline::decode(message)?;
        let positions = self
            .setup
            .instruction_positions
            .iter()
            .map(|position| usize::from(*position))
            .collect::<BTreeSet<_>>();
        if positions.len() != self.setup.instruction_positions.len() || positions.is_empty() {
            return Err("wallet setup instruction positions".into());
        }
        let retain =
            |position: usize, ix: &solana_message::compiled_instruction::CompiledInstruction| {
                positions.contains(&position)
                    || self
                        .keys
                        .get(usize::from(ix.program_id_index))
                        .is_some_and(|program| program == COMPUTE)
            };
        match &mut msg {
            solana_message::VersionedMessage::Legacy(value) => {
                value.instructions = value
                    .instructions
                    .iter()
                    .enumerate()
                    .filter(|(position, ix)| retain(*position, ix))
                    .map(|(_, ix)| ix.clone())
                    .collect();
            }
            solana_message::VersionedMessage::V0(value) => {
                value.instructions = value
                    .instructions
                    .iter()
                    .enumerate()
                    .filter(|(position, ix)| retain(*position, ix))
                    .map(|(_, ix)| ix.clone())
                    .collect();
            }
        }
        if msg.instructions().len() < positions.len()
            || msg.instructions().iter().any(|ix| {
                self.keys
                    .get(usize::from(ix.program_id_index))
                    .is_none_or(|program| {
                        program != COMPUTE
                            && program != SYSTEM
                            && program != ASSOCIATED_TOKEN
                            && program != TOKEN
                            && program != &self.program
                    })
            })
        {
            return Err("wallet setup prefix reconstruction".into());
        }
        let bytes = msg.serialize();
        pipeline::decode(&bytes)?;
        Ok(Some(bytes))
    }

    pub(crate) fn setup_only_addresses(&self) -> Vec<String> {
        if !self.setup.active() {
            return Vec::new();
        }
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for address in std::iter::once(&self.setup.owner)
            .chain(self.setup.created_tokens.iter().map(|token| &token.account))
            .chain(self.setup.wrap_token.iter())
            .chain(self.setup.created_nonce.iter())
        {
            if seen.insert(address.clone()) {
                out.push(address.clone());
            }
        }
        out
    }

    /// Verify the setup-only post-state before it can seed native lowering.
    /// Every balance is derived from the original coherent execution bank.
    pub(crate) fn verify_setup_only(
        &self,
        snapshot: &Snapshot,
        returned: &[crate::feed::Account],
    ) -> Result<()> {
        if !self.setup.active() {
            return if returned.is_empty() {
                Ok(())
            } else {
                Err("unexpected wallet setup observations".into())
            };
        }
        let addresses = self.setup_only_addresses();
        if returned.len() != addresses.len()
            || returned
                .iter()
                .zip(&addresses)
                .any(|(value, address)| &value.key != address)
        {
            return Err("wallet setup-only account set".into());
        }
        let find = |address: &str| {
            returned
                .iter()
                .find(|value| value.key == address)
                .ok_or_else(|| "wallet setup-only account missing".to_string())
        };
        let before_owner = account(snapshot, &self.setup.owner)?;
        let after_owner = find(&self.setup.owner)?;
        if before_owner.owner != SYSTEM
            || before_owner.executable
            || !before_owner.data.is_empty()
            || after_owner.owner != SYSTEM
            || after_owner.executable
            || !after_owner.data.is_empty()
        {
            return Err("wallet setup-only payer state".into());
        }
        let mut rent = 0u64;
        for binding in &self.setup.created_tokens {
            explicit_empty(snapshot, &binding.account)?;
            let value = find(&binding.account)?;
            let amount = raw_amount(value, binding)?;
            let expected = if self.setup.wrap_token.as_deref() == Some(&binding.account) {
                self.setup.wrap_lamports
            } else {
                0
            };
            if amount != expected {
                return Err("wallet setup-only created token amount".into());
            }
            let reserve = if binding.mint == WSOL {
                value
                    .lamports
                    .checked_sub(amount)
                    .ok_or("wallet setup-only native reserve")?
            } else {
                value.lamports
            };
            if reserve == 0 {
                return Err("wallet setup-only ATA rent".into());
            }
            rent = rent
                .checked_add(reserve)
                .ok_or("wallet setup-only rent overflow")?;
        }
        if let Some(address) = &self.setup.wrap_token {
            if !self.setup.created(address) {
                let binding = TokenBinding {
                    account: address.clone(),
                    mint: WSOL.into(),
                    owner: self.setup.owner.clone(),
                    token_program: TOKEN.into(),
                    decimals: 9,
                };
                let before = account(snapshot, address)?;
                let after = find(address)?;
                let expected = raw_amount(before, &binding)?
                    .checked_add(self.setup.wrap_lamports)
                    .ok_or("wallet setup-only wrap amount overflow")?;
                if raw_amount(after, &binding)? != expected
                    || after.lamports
                        != before
                            .lamports
                            .checked_add(self.setup.wrap_lamports)
                            .ok_or("wallet setup-only wrap lamport overflow")?
                {
                    return Err("wallet setup-only existing native account".into());
                }
            }
        }
        if let Some(nonce) = &self.setup.created_nonce {
            explicit_empty(snapshot, nonce)?;
            let value = find(nonce)?;
            if value.owner != self.program
                || value.executable
                || value.data.len() != 64
                || &value.data[..8] != b"SKEWSEQ1"
                || value.data[8..40] != key(&self.setup.owner)?.to_bytes()
                || integer(&value.data, 40)? != self.sequence
                || value.data[48..].iter().any(|byte| *byte != 0)
                || value.lamports == 0
            {
                return Err("wallet setup-only nonce state".into());
            }
            rent = rent
                .checked_add(value.lamports)
                .ok_or("wallet setup-only nonce rent overflow")?;
        }
        let debit = before_owner
            .lamports
            .checked_sub(after_owner.lamports)
            .ok_or("wallet setup-only payer increase")?;
        if debit
            != rent
                .checked_add(self.setup.wrap_lamports)
                .ok_or("wallet setup-only debit overflow")?
        {
            return Err("wallet setup-only payer debit".into());
        }
        Ok(())
    }

    pub(crate) fn verify_simulated_setup(
        &self,
        snapshot: &Snapshot,
        token_after: &[u64],
        returned: &[crate::feed::Account],
    ) -> Result<()> {
        if !self.setup.active() {
            return Ok(());
        }
        let find = |address: &str| {
            returned
                .iter()
                .find(|account| account.key == address)
                .ok_or_else(|| "wallet setup simulation account missing".to_string())
        };
        let before_owner = account(snapshot, &self.setup.owner)?;
        let after_owner = find(&self.setup.owner)?;
        if before_owner.owner != SYSTEM
            || before_owner.executable
            || !before_owner.data.is_empty()
            || after_owner.owner != SYSTEM
            || after_owner.executable
            || !after_owner.data.is_empty()
        {
            return Err("wallet setup payer state".into());
        }
        let observations = self.observation_bindings();
        if observations.len() != token_after.len() {
            return Err("wallet setup token observation count".into());
        }
        let mut rent = 0u64;
        for binding in &self.setup.created_tokens {
            let index = observations
                .iter()
                .position(|candidate| candidate.account == binding.account)
                .ok_or("created ATA observation missing")?;
            let value = find(&binding.account)?;
            let reserve = if binding.mint == WSOL {
                value
                    .lamports
                    .checked_sub(token_after[index])
                    .ok_or("native ATA reserve")?
            } else {
                value.lamports
            };
            rent = rent
                .checked_add(reserve)
                .ok_or("wallet setup rent overflow")?;
        }
        if let Some(nonce) = &self.setup.created_nonce {
            explicit_empty(snapshot, nonce)?;
            let value = find(nonce)?;
            if value.owner != self.program
                || value.executable
                || value.data.len() != 64
                || &value.data[..8] != b"SKEWSEQ1"
                || value.data[8..40] != key(&self.setup.owner)?.to_bytes()
                || integer(&value.data, 40)?
                    != self
                        .sequence
                        .checked_add(1)
                        .ok_or("nonce sequence overflow")?
                || value.data[48..].iter().any(|byte| *byte != 0)
            {
                return Err("initialized nonce simulation state".into());
            }
            rent = rent
                .checked_add(value.lamports)
                .ok_or("wallet setup nonce rent overflow")?;
        }
        let debit = before_owner
            .lamports
            .checked_sub(after_owner.lamports)
            .ok_or("wallet setup payer lamport increase")?;
        if debit
            != rent
                .checked_add(self.setup.wrap_lamports)
                .ok_or("wallet setup debit overflow")?
        {
            return Err("wallet setup simulation lamport debit".into());
        }
        Ok(())
    }

    fn verify_landed_setup(&self, meta: &Value, token_after: &[u64]) -> Result<()> {
        if !self.setup.active() {
            return Ok(());
        }
        let pre = meta["preBalances"]
            .as_array()
            .filter(|rows| rows.len() == self.keys.len())
            .ok_or("wallet setup pre-lamport balances")?;
        let post = meta["postBalances"]
            .as_array()
            .filter(|rows| rows.len() == self.keys.len())
            .ok_or("wallet setup post-lamport balances")?;
        let index = |address: &str| {
            self.keys
                .iter()
                .position(|key| key == address)
                .ok_or_else(|| "wallet setup landed account missing".to_string())
        };
        let lamports = |rows: &[Value], address: &str| {
            rows[index(address)?]
                .as_u64()
                .ok_or_else(|| "wallet setup landed lamport value".to_string())
        };
        let observations = self.observation_bindings();
        if observations.len() != token_after.len() {
            return Err("wallet setup landed token observation count".into());
        }
        let mut rent = 0u64;
        for binding in &self.setup.created_tokens {
            if lamports(pre, &binding.account)? != 0 {
                return Err("created ATA existed before landed transaction".into());
            }
            let token_index = observations
                .iter()
                .position(|candidate| candidate.account == binding.account)
                .ok_or("created ATA landed observation missing")?;
            let reserve = if binding.mint == WSOL {
                lamports(post, &binding.account)?
                    .checked_sub(token_after[token_index])
                    .ok_or("landed native ATA reserve")?
            } else {
                lamports(post, &binding.account)?
            };
            rent = rent
                .checked_add(reserve)
                .ok_or("landed setup rent overflow")?;
        }
        if let Some(nonce) = &self.setup.created_nonce {
            if lamports(pre, nonce)? != 0 || lamports(post, nonce)? == 0 {
                return Err("landed nonce creation balance".into());
            }
            rent = rent
                .checked_add(lamports(post, nonce)?)
                .ok_or("landed nonce rent overflow")?;
        }
        let owner_debit = lamports(pre, &self.setup.owner)?
            .checked_sub(lamports(post, &self.setup.owner)?)
            .ok_or("landed setup payer lamport increase")?;
        let expected = rent
            .checked_add(self.setup.wrap_lamports)
            .and_then(|amount| amount.checked_add(meta["fee"].as_u64()?))
            .ok_or("landed setup debit overflow")?;
        if owner_debit != expected {
            return Err("landed wallet setup lamport debit".into());
        }
        Ok(())
    }

    /// Same check is used for simulation and a finalized chain receipt.
    pub(crate) fn outcome(
        &self,
        return_data: &Value,
        before: &[u64],
        after: &[u64],
        cu: u64,
    ) -> Result<(u64, Vec<Value>)> {
        if return_data["programId"].as_str() != Some(&self.program)
            || return_data["data"][1].as_str() != Some("base64")
        {
            return Err("exposure return program/encoding".into());
        }
        let d = STANDARD
            .decode(
                return_data["data"][0]
                    .as_str()
                    .ok_or("missing exposure return data")?,
            )
            .map_err(|_| "exposure return base64")?;
        let header = match self.opcode {
            13 => 40,
            18 => 56,
            _ => 48,
        };
        let tag = if self.opcode == 13 {
            b"SKEWEXP1"
        } else if self.opcode == 18 {
            if self
                .funding
                .as_ref()
                .is_some_and(|funding| funding.version == 2)
            {
                b"SKEWMSF2"
            } else {
                b"SKEWMSF1"
            }
        } else if self.direct_reflow {
            b"SKEWMSR1"
        } else {
            b"SKEWMSH1"
        };
        if d.len() != header + self.products.len() * 16 + self.sellers.len() * 24
            || &d[..8] != tag
            || integer(&d, 8)? != self.sequence
            || integer(&d, 16)? != self.input
            || integer(&d, 32)? != self.products.len() as u64
            || (self.opcode != 13 && integer(&d, 40)? != self.sellers.len() as u64)
            || before.len() != self.observation_bindings().len()
            || before.len() != after.len()
            || before[0]
                .checked_add(self.setup.wrap_lamports)
                .and_then(|funded| funded.checked_sub(after[0]))
                != Some(self.input)
            || cu == 0
            || cu > self.maximum_cu
        {
            return Err("exposure receipt shape/input/CU".into());
        }
        if let Some(funding) = &self.funding {
            if self.opcode != 18
                || !matches!(funding.version, 1 | 2)
                || integer(&d, 48)? < funding.minimum_cash
                || integer(&d, 48)? > stocklana_adapters::MAX_INPUT
            {
                return Err("funded cash receipt amount".into());
            }
            let start = 1 + self.products.len() + 2 * self.sellers.len();
            if before[start..] != after[start..] {
                return Err("preexisting funding assets changed".into());
            }
        } else if self.opcode == 18 {
            return Err("funding receipt without binding".into());
        }
        let mut total = 0u64;
        let mut positive_products = 0usize;
        let mut rows = Vec::new();
        for (i, p) in self.products.iter().enumerate() {
            let raw = integer(&d, header + i * 16)?;
            let exposure = integer(&d, header + i * 16 + 8)?;
            if (raw == 0) != (exposure == 0) || after[i + 1].checked_sub(before[i + 1]) != Some(raw)
            {
                return Err("exposure product balance mismatch".into());
            }
            positive_products += usize::from(raw > 0);
            if p.model == 0 && raw > 0 {
                let fixed = skew_native::ScaledUiAmount {
                    decimals: p.token.decimals,
                    multiplier_q32: 1u64 << 32,
                    next_multiplier_effective_timestamp: i64::MAX,
                    next_multiplier_q32: 1u64 << 32,
                };
                if fixed
                    .exposure_q32(raw, p.numerator, p.denominator, p.conservative_bps)
                    .map_err(|_| "exposure conversion overflow")?
                    != exposure
                {
                    return Err("fixed exposure arithmetic mismatch".into());
                }
            }
            total = total
                .checked_add(exposure)
                .ok_or("aggregate exposure overflow")?;
            rows.push(json!({"productId":hex(&p.product_id),"issuer":p.issuer,"mint":p.token.mint,"rawAtoms":raw.to_string(),"exposureQ32":exposure.to_string()}));
        }
        if positive_products == 0 || integer(&d, 24)? != total || total < self.floor {
            return Err("aggregate exposure floor/sum".into());
        }
        for (i, s) in self.sellers.iter().enumerate() {
            let offset = header + self.products.len() * 16 + i * 24;
            let b = 1 + self.products.len() + i * 2;
            if integer(&d, offset)? != s.sequence
                || integer(&d, offset + 8)? != s.stock_atoms
                || integer(&d, offset + 16)? != s.cash_atoms
                || before[b].checked_sub(after[b]) != Some(s.stock_atoms)
                || after[b + 1].checked_sub(before[b + 1]) != Some(s.cash_atoms)
            {
                return Err("internal seller receipt mismatch".into());
            }
        }
        let core = self.balance_bindings().len();
        if before[core..].iter().any(|amount| *amount != 0)
            || after[core..].iter().any(|amount| *amount != 0)
        {
            return Err("created intermediate ATA retained funds".into());
        }
        Ok((total, rows))
    }

    pub fn fetch(&self, rpc: &Rpc, entry: &Entry) -> Result<VerifiedExposure> {
        if !matches!(entry.phase, Phase::Finalized | Phase::Reconciled) {
            return Err("economic receipt not finalized".into());
        }
        rpc.check_genesis()?;
        let value=rpc.call("getTransaction",json!([entry.signature,{"encoding":"base64","commitment":"finalized","maxSupportedTransactionVersion":0}]))?;
        self.verify(entry, &value)
    }

    /// Verify metadata supplied by the configured trusted RPC. Public callers
    /// cannot promote JSON to a finalized receipt through the API.
    pub(crate) fn verify(&self, entry: &Entry, value: &Value) -> Result<VerifiedExposure> {
        if !matches!(entry.phase, Phase::Finalized | Phase::Reconciled)
            || entry.message_hash != self.message_hash
            || entry.resources != self.resources
            || value["transaction"][1].as_str() != Some("base64")
        {
            return Err("economic receipt approval/phase".into());
        }
        let wire = STANDARD
            .decode(
                value["transaction"][0]
                    .as_str()
                    .ok_or("economic transaction missing")?,
            )
            .map_err(|_| "economic wire base64")?;
        if wire != entry.wire {
            return Err("economic receipt wire substitution".into());
        }
        let authorized = crate::sender::authorize(
            &wire,
            crate::sender::Authorization {
                intent_id: entry.id.clone(),
                message_hash: self.message_hash,
                last_valid_height: entry.last_valid_height,
                resources: self.resources.clone(),
            },
        )?;
        if authorized.signature != entry.signature {
            return Err("economic receipt signature".into());
        }
        let message = pipeline::decode(&wire[1 + usize::from(wire[0]) * 64..])?;
        let meta = value
            .get("meta")
            .filter(|v| v.is_object())
            .ok_or("economic metadata missing")?;
        if !meta
            .get("err")
            .ok_or("economic execution status missing")?
            .is_null()
        {
            return Err("economic execution failed".into());
        }
        let mut keys = message
            .static_account_keys()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        for (kind, count) in [("writable", true), ("readonly", false)] {
            let expected = message.address_table_lookups().map_or(0, |tables| {
                tables
                    .iter()
                    .map(|t| {
                        if count {
                            t.writable_indexes.len()
                        } else {
                            t.readonly_indexes.len()
                        }
                    })
                    .sum()
            });
            let loaded = meta["loadedAddresses"][kind].as_array();
            if loaded.map_or(0, Vec::len) != expected {
                return Err("economic ALT address count".into());
            }
            for value in loaded.into_iter().flatten() {
                keys.push(value.as_str().ok_or("economic ALT address")?.into());
            }
        }
        if keys != self.keys {
            return Err("economic resolved account substitution".into());
        }
        let amount = |kind: &str, b: &TokenBinding| -> Result<u64> {
            let index = keys
                .iter()
                .position(|k| k == &b.account)
                .ok_or("economic balance account missing")?;
            let rows = meta[kind]
                .as_array()
                .ok_or("economic token balances missing")?;
            let mut rows = rows
                .iter()
                .filter(|v| v["accountIndex"].as_u64() == Some(index as u64));
            let Some(row) = rows.next() else {
                return if kind == "preTokenBalances" && self.setup.created(&b.account) {
                    Ok(0)
                } else {
                    Err("economic token balance missing".into())
                };
            };
            if rows.next().is_some()
                || row["mint"].as_str() != Some(&b.mint)
                || row["owner"].as_str() != Some(&b.owner)
                || row["programId"].as_str() != Some(&b.token_program)
                || row["uiTokenAmount"]["decimals"].as_u64() != Some(u64::from(b.decimals))
            {
                return Err("economic token identity/duplicate".into());
            }
            parse_amount(&row["uiTokenAmount"]["amount"])
        };
        let bindings = self.observation_bindings();
        let before = bindings
            .iter()
            .map(|b| amount("preTokenBalances", b))
            .collect::<Result<Vec<_>>>()?;
        let after = bindings
            .iter()
            .map(|b| amount("postTokenBalances", b))
            .collect::<Result<Vec<_>>>()?;
        let cu = meta["computeUnitsConsumed"]
            .as_u64()
            .ok_or("economic CU missing")?;
        let (exposure, products) = self.outcome(&meta["returnData"], &before, &after, cu)?;
        self.verify_landed_setup(meta, &after)?;
        let slot = value["slot"]
            .as_u64()
            .filter(|s| *s >= self.prepared_slot && *s <= self.deadline)
            .ok_or("economic receipt slot")?;
        Ok(VerifiedExposure {
            signature: entry.signature.clone(),
            instrument: self.instrument.clone(),
            input: self.input,
            exposure,
            products,
            cu,
            fee: meta["fee"].as_u64().ok_or("economic fee missing")?,
            slot,
        })
    }
}

fn parse_amount(value: &Value) -> Result<u64> {
    let text = value.as_str().ok_or("raw token amount string")?;
    if text.is_empty()
        || text.len() > 20
        || !text.bytes().all(|b| b.is_ascii_digit())
        || (text.len() > 1 && text.starts_with('0'))
    {
        return Err("noncanonical raw amount".into());
    }
    text.parse().map_err(|_| "raw amount overflow".into())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod direct_reflow_vector;
#[cfg(test)]
mod funded_vector;
#[cfg(test)]
mod sbf_vector;
#[cfg(test)]
pub(crate) mod tests;
