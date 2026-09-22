//! Typed ATA + native SOL setup. These unsigned instructions are preparatory,
//! never an execution admission. Simulate them at the same bank, use the observed
//! initialized wallet accounts for lowering, then simulate the entire exact wire.
use crate::{
    feed::{Account, Snapshot},
    swap_wire::{AccountView, TokenAsset},
    Result,
};
use solana_instruction::{AccountMeta, Instruction};
use solana_message::AddressLookupTableAccount;
use solana_pubkey::{pubkey, Pubkey};
use std::collections::BTreeSet;

const SYSTEM: Pubkey = Pubkey::new_from_array([0; 32]);
const ATA: Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
const TOKEN: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const TOKEN22: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
const WSOL: Pubkey = pubkey!("So11111111111111111111111111111111111111112");

// Four issuer products plus funding input and cash intermediate. These bounds
// derive from the admitted graph, not an arbitrary top-level action allowance.
pub const MAX_WALLET_ASSETS: usize = 6;
pub const MAX_WALLET_SETUP_INSTRUCTIONS: usize = MAX_WALLET_ASSETS + 2 + 1;
pub const MAX_STOCK_TRANSACTION_INSTRUCTIONS: usize = MAX_WALLET_SETUP_INSTRUCTIONS + 3 + 1;

#[derive(Clone)]
pub struct WalletSetup {
    owner: Pubkey,
    assets: Vec<TokenAsset>,
    initial: Vec<Option<u64>>,
    wrap: Option<(usize, u64)>,
    nonce: Option<(Pubkey, Pubkey, u64, bool)>,
    instructions: Vec<Instruction>,
    payout: Option<crate::native_payout::NativePayout>,
}
fn require(ok: bool, error: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(error.into())
    }
}
fn amount(account: AccountView<'_>, asset: TokenAsset, owner: Pubkey) -> Result<u64> {
    let d = account.data;
    require(
        account.owner == asset.token_program && !account.executable && d.len() >= 165,
        "wallet setup token owner/layout",
    )?;
    require(
        d[..32] == asset.mint.to_bytes()
            && d[32..64] == owner.to_bytes()
            && d[108] == 1
            && d[72..76] == [0; 4]
            && d[121..129] == [0; 8]
            && d[129..133] == [0; 4],
        "wallet setup token authority/state",
    )?;
    Ok(u64::from_le_bytes(
        d[64..72].try_into().map_err(|_| "wallet amount")?,
    ))
}
fn account_view(account: &Account) -> Result<AccountView<'_>> {
    Ok(AccountView {
        owner: account
            .owner
            .parse()
            .map_err(|_| "wallet setup returned owner")?,
        executable: account.executable,
        data: &account.data,
    })
}
impl WalletSetup {
    /// Absent accounts must be explicit canonical system-owned empty accounts
    /// from the bank reader. A network failure is never account absence.
    pub fn plan<'a>(
        owner: Pubkey,
        assets: &[TokenAsset],
        wrap_lamports: Option<u64>,
        read: impl Fn(&Pubkey) -> Result<AccountView<'a>>,
    ) -> Result<Self> {
        require(
            owner != SYSTEM && (1..=MAX_WALLET_ASSETS).contains(&assets.len()),
            "wallet setup bounds",
        )?;
        let payer = read(&owner)?;
        require(
            payer.owner == SYSTEM && payer.data.is_empty() && !payer.executable,
            "wallet setup payer identity",
        )?;
        let mut seen = BTreeSet::new();
        let mut initial = Vec::new();
        let mut instructions = Vec::new();
        let mut wrap_index = None;
        for (index, asset) in assets.iter().enumerate() {
            require(
                [TOKEN, TOKEN22].contains(&asset.token_program) && seen.insert(asset.mint),
                "wallet setup asset identity",
            )?;
            let expected = Pubkey::find_program_address(
                &[
                    owner.as_ref(),
                    asset.token_program.as_ref(),
                    asset.mint.as_ref(),
                ],
                &ATA,
            )
            .0;
            require(asset.token == expected, "wallet setup ATA binding")?;
            let mint = read(&asset.mint)?;
            require(
                mint.owner == asset.token_program
                    && !mint.executable
                    && mint.data.len() >= 82
                    && mint.data[45] == 1,
                "wallet setup mint binding",
            )?;
            require(
                read(&asset.token_program)?.executable,
                "wallet setup token executable",
            )?;
            let token = read(&asset.token)?;
            if token.owner == SYSTEM && token.data.is_empty() && !token.executable {
                require(read(&ATA)?.executable, "wallet setup ATA executable")?;
                instructions.push(Instruction {
                    program_id: ATA,
                    data: vec![1],
                    accounts: vec![
                        AccountMeta::new(owner, true),
                        AccountMeta::new(asset.token, false),
                        AccountMeta::new_readonly(owner, false),
                        AccountMeta::new_readonly(asset.mint, false),
                        AccountMeta::new_readonly(SYSTEM, false),
                        AccountMeta::new_readonly(asset.token_program, false),
                    ],
                });
                initial.push(None);
            } else {
                initial.push(Some(amount(token, *asset, owner)?));
            }
            if asset.mint == WSOL {
                require(
                    asset.token_program == TOKEN,
                    "wallet setup native token program",
                )?;
                wrap_index = Some(index);
            }
        }
        let wrap = if let Some(lamports) = wrap_lamports {
            require(
                lamports > 0 && lamports <= stocklana_adapters::MAX_INPUT,
                "wallet setup wrap bound",
            )?;
            let index = wrap_index.ok_or("wallet setup missing wSOL")?;
            initial[index]
                .unwrap_or(0)
                .checked_add(lamports)
                .ok_or("wallet setup wrap overflow")?;
            let mut data = 2u32.to_le_bytes().to_vec();
            data.extend_from_slice(&lamports.to_le_bytes());
            instructions.push(Instruction {
                program_id: SYSTEM,
                data,
                accounts: vec![
                    AccountMeta::new(owner, true),
                    AccountMeta::new(assets[index].token, false),
                ],
            });
            instructions.push(Instruction {
                program_id: TOKEN,
                data: vec![17],
                accounts: vec![AccountMeta::new(assets[index].token, false)],
            });
            Some((index, lamports))
        } else {
            None
        };
        // Creates plus transfer/SyncNative. Reserve the replay nonce separately;
        // compute limit, heap, priority fee and settlement belong to the caller.
        require(instructions.len() < MAX_WALLET_SETUP_INSTRUCTIONS, "wallet setup instruction bound")?;
        Ok(Self {
            owner,
            assets: assets.to_vec(),
            initial,
            wrap,
            nonce: None,
            instructions,
            payout: None,
        })
    }
    pub fn instructions(&self) -> &[Instruction] {
        &self.instructions
    }
    pub fn with_native_payout<'a>(mut self, payout:crate::native_payout::NativePayout,
        read:impl Fn(&Pubkey)->Result<AccountView<'a>>) -> Result<Self> {
        let asset=payout.asset()?;
        require(self.payout.is_none() && self.wrap.is_none() && payout.owner()?==self.owner
            && self.assets.len()<=3 && self.assets.iter().all(|a|a.token!=asset.token&&a.mint!=WSOL),"wallet payout binding")?;
        payout.validate_absent(read(&asset.token)?)?;
        self.instructions.extend(payout.setup()?);
        require(self.instructions.len()<=MAX_WALLET_SETUP_INSTRUCTIONS,"wallet payout instruction bound")?;
        self.payout=Some(payout);Ok(self)
    }
    pub fn native_payout(&self)->Option<&crate::native_payout::NativePayout>{self.payout.as_ref()}
    /// A first-time wallet also needs its settlement replay-protection account.
    /// Initialization is in the same transaction as the trade and rolls back if
    /// any later action fails. An existing nonce can never be reset here.
    pub fn with_nonce<'a>(
        mut self,
        program: Pubkey,
        sequence: u64,
        read: impl Fn(&Pubkey) -> Result<AccountView<'a>>,
    ) -> Result<Self> {
        require(
            self.nonce.is_none() && program != SYSTEM && read(&program)?.executable,
            "wallet setup settlement identity",
        )?;
        let address =
            Pubkey::find_program_address(&[b"stocklana", self.owner.as_ref()], &program).0;
        let a = read(&address)?;
        let create = a.owner == SYSTEM && !a.executable && a.data.is_empty();
        if create {
            require(sequence == 0, "wallet setup initial nonce sequence")?;
            self.instructions.insert(
                0,
                Instruction {
                    program_id: program,
                    data: vec![0],
                    accounts: vec![
                        AccountMeta::new(self.owner, true),
                        AccountMeta::new(address, false),
                        AccountMeta::new_readonly(SYSTEM, false),
                    ],
                },
            );
        } else {
            Self::nonce_state(a, program, self.owner, sequence)?;
        }
        require(
            self.instructions.len() <= MAX_WALLET_SETUP_INSTRUCTIONS,
            "wallet setup full instruction bound",
        )?;
        self.nonce = Some((address, program, sequence, create));
        Ok(self)
    }

    /// Read the replay sequence from the same execution bank used for lowering.
    /// An explicitly absent PDA starts at zero; callers never supply a guessed
    /// sequence from a quote API or browser cache.
    pub fn with_observed_nonce<'a>(
        self,
        program: Pubkey,
        read: impl Copy + Fn(&Pubkey) -> Result<AccountView<'a>>,
    ) -> Result<(Self, u64)> {
        require(program != SYSTEM, "wallet setup settlement identity")?;
        let address =
            Pubkey::find_program_address(&[b"stocklana", self.owner.as_ref()], &program).0;
        let account = read(&address)?;
        let sequence = if account.owner == SYSTEM && !account.executable && account.data.is_empty()
        {
            0
        } else {
            require(
                account.owner == program
                    && !account.executable
                    && account.data.len() == 64
                    && &account.data[..8] == b"SKEWSEQ1"
                    && account.data[8..40] == self.owner.to_bytes(),
                "wallet setup observed nonce identity",
            )?;
            u64::from_le_bytes(
                account.data[40..48]
                    .try_into()
                    .map_err(|_| "wallet setup observed nonce layout")?,
            )
        };
        Ok((self.with_nonce(program, sequence, read)?, sequence))
    }
    fn nonce_state(
        a: AccountView<'_>,
        program: Pubkey,
        owner: Pubkey,
        sequence: u64,
    ) -> Result<()> {
        require(
            a.owner == program
                && !a.executable
                && a.data.len() == 64
                && &a.data[..8] == b"SKEWSEQ1"
                && a.data[8..40] == owner.to_bytes()
                && u64::from_le_bytes(
                    a.data[40..48]
                        .try_into()
                        .map_err(|_| "wallet nonce layout")?,
                ) == sequence,
            "wallet setup nonce binding",
        )
    }
    pub fn created_nonce(&self) -> Option<Pubkey> {
        self.nonce
            .filter(|(_, _, _, created)| *created)
            .map(|(address, _, _, _)| address)
    }
    pub fn created_assets(&self) -> impl Iterator<Item = TokenAsset> + '_ {
        self.assets
            .iter()
            .zip(&self.initial)
            .filter_map(|(asset, old)| old.is_none().then_some(*asset))
    }
    pub fn wrap_lamports(&self) -> u64 {
        self.wrap.map_or(0, |(_, amount)| amount)
    }

    pub fn setup_only_addresses(&self) -> Vec<Pubkey> {
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for address in std::iter::once(self.owner)
            .chain(self.assets.iter().map(|asset| asset.token))
            .chain(self.nonce.map(|value| value.0))
            .chain(self.payout.as_ref().and_then(|p|p.asset().ok()).map(|a|a.token))
        {
            if seen.insert(address) {
                out.push(address);
            }
        }
        out
    }

    pub fn compile_setup_only(
        &self,
        lookup_tables: &[AddressLookupTableAccount],
        recent_blockhash: [u8; 32],
        compute_unit_limit: u32,
    ) -> Result<Option<Vec<u8>>> {
        if self.instructions.is_empty() {
            return Ok(None);
        }
        require(
            compute_unit_limit > 0 && compute_unit_limit <= 1_400_000,
            "wallet setup compute ceiling",
        )?;
        let mut data = vec![2];
        data.extend_from_slice(&compute_unit_limit.to_le_bytes());
        let mut instructions = vec![Instruction {
            program_id: pubkey!("ComputeBudget111111111111111111111111111111"),
            accounts: Vec::new(),
            data,
        }];
        instructions.extend_from_slice(&self.instructions);
        Ok(Some(crate::onebook_wire::compile_unsigned_v0(
            self.owner,
            &instructions,
            lookup_tables,
            recent_blockhash,
        )?))
    }

    pub fn validate_setup_only_message(&self, snapshot: &Snapshot, message: &[u8]) -> Result<()> {
        let decoded = crate::pipeline::decode(message)?;
        let keys = crate::pipeline::resolved(&decoded, snapshot)?;
        let required = usize::from(decoded.header().num_required_signatures);
        require(
            required == 1
                && decoded.static_account_keys().first() == Some(&self.owner)
                && decoded.instructions().len() == self.instructions.len() + 1,
            "wallet setup message header",
        )?;
        let compute = &decoded.instructions()[0];
        let compute_program = keys
            .get(usize::from(compute.program_id_index))
            .ok_or("wallet setup compute program")?;
        require(
            compute_program == "ComputeBudget111111111111111111111111111111"
                && compute.accounts.is_empty()
                && compute.data.len() == 5
                && compute.data[0] == 2
                && (1..=1_400_000).contains(&u32::from_le_bytes(
                    compute.data[1..5]
                        .try_into()
                        .map_err(|_| "wallet setup compute data")?,
                )),
            "wallet setup compute instruction",
        )?;
        // Message privileges are the union across all occurrences. In an ATA
        // instruction the payer and token owner are the same wallet but the
        // individual metas differ; comparing each meta to the union rejects
        // every legitimate first-use setup.
        let mut privileges=std::collections::BTreeMap::<Pubkey,(bool,bool)>::new();
        for ix in &self.instructions {
            for meta in &ix.accounts {
                let entry=privileges.entry(meta.pubkey).or_default();
                entry.0|=meta.is_signer;entry.1|=meta.is_writable;
            }
        }
        for (compiled, expected) in decoded.instructions()[1..].iter().zip(&self.instructions) {
            require(
                keys.get(usize::from(compiled.program_id_index))
                    == Some(&expected.program_id.to_string())
                    && compiled.data == expected.data
                    && compiled.accounts.len() == expected.accounts.len(),
                "wallet setup message instruction",
            )?;
            for (index, meta) in compiled.accounts.iter().zip(&expected.accounts) {
                let position = usize::from(*index);
                let (signer,writable)=privileges[&meta.pubkey];
                require(
                    keys.get(position) == Some(&meta.pubkey.to_string())
                        && (position < required) == signer
                        && decoded.is_maybe_writable(position, None) == writable,
                    "wallet setup message account privilege",
                )?;
            }
        }
        Ok(())
    }

    /// Admit a setup-only simulation as a lowering projection. The returned
    /// bank keeps the original generation/hash/currentness because it is never
    /// settlement evidence; the complete setup-plus-trade wire must still be
    /// simulated against `original`.
    pub fn lowering_projection(
        &self,
        original: &Snapshot,
        returned: &[Account],
        simulated_fee: u64,
    ) -> Result<Snapshot> {
        let addresses = self.setup_only_addresses();
        require(
            returned.len() == addresses.len()
                && returned
                    .iter()
                    .zip(&addresses)
                    .all(|(account, address)| account.key == address.to_string()),
            "wallet setup simulation account set",
        )?;
        let original_account = |address: &Pubkey| -> Result<&Account> {
            let name = address.to_string();
            let mut rows = original
                .accounts
                .iter()
                .filter(|account| account.key == name);
            let value = rows.next().ok_or("wallet setup original account")?;
            require(
                rows.next().is_none(),
                "wallet setup ambiguous original account",
            )?;
            Ok(value)
        };
        let returned_account = |address: &Pubkey| -> Result<&Account> {
            returned
                .iter()
                .find(|account| account.key == address.to_string())
                .ok_or_else(|| "wallet setup returned account".into())
        };
        self.verify_initialized(|address| account_view(returned_account(address)?))?;
        let before_owner = original_account(&self.owner)?;
        let after_owner = returned_account(&self.owner)?;
        require(
            before_owner.owner == SYSTEM.to_string()
                && !before_owner.executable
                && before_owner.data.is_empty()
                && after_owner.owner == SYSTEM.to_string()
                && !after_owner.executable
                && after_owner.data.is_empty(),
            "wallet setup projection payer",
        )?;
        let same = |before: &Account, after: &Account| {
            before.owner == after.owner
                && before.executable == after.executable
                && before.lamports == after.lamports
                && before.data == after.data
        };
        let mut rent = 0u64;
        if let Some(payout)=&self.payout {
            let address=payout.asset()?.token;
            let before=original_account(&address)?;let after=returned_account(&address)?;
            require(before.lamports==0 && after.lamports==payout.rent(),"wallet payout rent")?;
            payout.validate_absent(account_view(before)?)?;
            payout.validate_initialized(account_view(after)?)?;
            rent=payout.rent();
        }
        for (index, asset) in self.assets.iter().enumerate() {
            let before = original_account(&asset.token)?;
            let after = returned_account(&asset.token)?;
            let wrapped = self
                .wrap
                .filter(|(position, _)| *position == index)
                .map_or(0, |(_, lamports)| lamports);
            if self.initial[index].is_none() {
                require(
                    before.owner == SYSTEM.to_string()
                        && !before.executable
                        && before.lamports == 0
                        && before.data.is_empty(),
                    "wallet setup projection ATA existed",
                )?;
                let current = amount(account_view(after)?, *asset, self.owner)?;
                let reserve = if asset.mint == WSOL {
                    after
                        .lamports
                        .checked_sub(current)
                        .ok_or("wallet setup projection native reserve")?
                } else {
                    after.lamports
                };
                require(reserve > 0, "wallet setup projection ATA rent")?;
                rent = rent
                    .checked_add(reserve)
                    .ok_or("wallet setup projection rent overflow")?;
            } else if wrapped == 0 {
                require(same(before, after), "wallet setup changed existing token")?;
            } else {
                let mut expected = before.data.clone();
                let current = self.initial[index]
                    .ok_or("wallet setup existing amount")?
                    .checked_add(wrapped)
                    .ok_or("wallet setup projection wrap overflow")?;
                expected[64..72].copy_from_slice(&current.to_le_bytes());
                require(
                    before.owner == after.owner
                        && before.executable == after.executable
                        && after.data == expected
                        && after.lamports == before.lamports.saturating_add(wrapped),
                    "wallet setup changed existing native token",
                )?;
            }
        }
        if let Some((nonce, _, _, created)) = self.nonce {
            let before = original_account(&nonce)?;
            let after = returned_account(&nonce)?;
            if created {
                require(
                    before.owner == SYSTEM.to_string()
                        && !before.executable
                        && before.lamports == 0
                        && before.data.is_empty()
                        && after.lamports > 0,
                    "wallet setup projection nonce creation",
                )?;
                rent = rent
                    .checked_add(after.lamports)
                    .ok_or("wallet setup projection nonce rent overflow")?;
            } else {
                require(same(before, after), "wallet setup changed existing nonce")?;
            }
        }
        let debit = before_owner
            .lamports
            .checked_sub(after_owner.lamports)
            .ok_or("wallet setup projection payer increase")?;
        require(
            debit
                == rent
                    .checked_add(self.wrap_lamports())
                    .and_then(|value|value.checked_add(simulated_fee))
                    .ok_or("wallet setup projection debit overflow")?,
            "wallet setup projection payer debit",
        )?;
        let mut projection = original.clone();
        for value in returned {
            let target = projection
                .accounts
                .iter_mut()
                .find(|account| account.key == value.key)
                .ok_or("wallet setup projection target")?;
            *target = value.clone();
        }
        Ok(projection)
    }

    /// Check the actual setup-only simulation result before it may supply the
    /// wallet accounts to the native compiler. Full-wire simulation still follows.
    pub fn verify_initialized<'a>(
        &self,
        read: impl Fn(&Pubkey) -> Result<AccountView<'a>>,
    ) -> Result<()> {
        if let Some(payout)=&self.payout {payout.validate_initialized(read(&payout.asset()?.token)?)?;}
        if let Some((address, program, sequence, _)) = self.nonce {
            Self::nonce_state(read(&address)?, program, self.owner, sequence)?;
        }
        for (index, asset) in self.assets.iter().enumerate() {
            let expected = self.initial[index]
                .unwrap_or(0)
                .checked_add(self.wrap.filter(|(i, _)| *i == index).map_or(0, |(_, q)| q))
                .ok_or("wallet setup expected overflow")?;
            require(
                amount(read(&asset.token)?, *asset, self.owner)? == expected,
                "wallet setup initialized amount mismatch",
            )?;
        }
        Ok(())
    }
}

/// Decode one immutable ALT from the final execution bank. This mirrors the
/// resolver admission rule, but returns the typed table used to compile the
/// exact wallet message.
pub fn frozen_lookup_table(
    snapshot: &Snapshot,
    address: Pubkey,
) -> Result<AddressLookupTableAccount> {
    let key = address.to_string();
    let mut rows = snapshot
        .accounts
        .iter()
        .filter(|account| account.key == key);
    let account = rows.next().ok_or("wallet setup ALT missing")?;
    require(rows.next().is_none(), "wallet setup ALT ambiguous")?;
    let data = &account.data;
    require(
        account.owner == "AddressLookupTab1e1111111111111111111111111"
            && !account.executable
            && data.len() >= 88
            && data[..4] == 1u32.to_le_bytes()
            && u64::from_le_bytes(
                data[4..12]
                    .try_into()
                    .map_err(|_| "wallet setup ALT layout")?,
            ) == u64::MAX
            && u64::from_le_bytes(
                data[12..20]
                    .try_into()
                    .map_err(|_| "wallet setup ALT layout")?,
            ) < snapshot.slot
            && data[21] == 0
            && (data.len() - 56) % 32 == 0,
        "wallet setup requires frozen active ALT",
    )?;
    let addresses = data[56..]
        .chunks_exact(32)
        .map(|bytes| {
            Ok(Pubkey::new_from_array(
                bytes.try_into().map_err(|_| "wallet setup ALT address")?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    require(
        !addresses.is_empty()
            && addresses.len() <= 256
            && addresses.iter().collect::<BTreeSet<_>>().len() == addresses.len(),
        "wallet setup ALT address set",
    )?;
    Ok(AddressLookupTableAccount {
        key: address,
        addresses,
    })
}
