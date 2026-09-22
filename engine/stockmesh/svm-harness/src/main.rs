//! Executes captured mainnet DEX ELFs, never mock/native DEX processors.
//! Synthetic wallet balances and fault mutations are explicitly labeled.
use mollusk_svm::{result::types::TransactionResult, Mollusk};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use solana_account::Account;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::{pubkey, Pubkey};
use std::{fs, path::PathBuf, str::FromStr};
use stocklana_adapters::{self as dex, PhoenixBook, PhoenixHeader, RaydiumPool};

const SETTLE: Pubkey = Pubkey::new_from_array([83; 32]);
const WALLET: Pubkey = Pubkey::new_from_array([71; 32]);
const USER_BASE: Pubkey = Pubkey::new_from_array([72; 32]);
const USER_QUOTE: Pubkey = Pubkey::new_from_array([73; 32]);
const TOKEN: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const PHOENIX: Pubkey = pubkey!("PhoeNiXZ8ByJGLkxNfZRnkUfjvmuYqLR89jjFHGqdXY");
const RAYDIUM: Pubkey = pubkey!("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C");
const START: u64 = 100_000_000_000_000;
const CU_LIMIT: u64 = 150_000;
type Accounts = Vec<(Pubkey, Account)>;
fn pk(s: &str) -> Pubkey {
    Pubkey::from_str(s).unwrap()
}
fn get<'a>(a: &'a Accounts, k: &Pubkey) -> &'a Account {
    &a.iter().find(|(key, _)| key == k).unwrap().1
}
fn get_mut<'a>(a: &'a mut Accounts, k: &Pubkey) -> &'a mut Account {
    &mut a.iter_mut().find(|(key, _)| key == k).unwrap().1
}
fn amount(a: &Accounts, k: &Pubkey) -> u64 {
    dex::token_amount(&get(a, k).data).unwrap()
}
fn token(mint: Pubkey) -> Account {
    let mut data = vec![0u8; 165];
    data[..32].copy_from_slice(mint.as_ref());
    data[32..64].copy_from_slice(WALLET.as_ref());
    data[64..72].copy_from_slice(&START.to_le_bytes());
    data[108] = 1;
    let lamports = if mint == pubkey!("So11111111111111111111111111111111111111112") {
        data[109..113].copy_from_slice(&1u32.to_le_bytes());
        data[113..121].copy_from_slice(&2_039_280u64.to_le_bytes());
        START + 2_039_280
    } else {
        2_039_280
    };
    Account {
        lamports,
        data,
        owner: TOKEN,
        executable: false,
        rent_epoch: 0,
    }
}
pub(crate) struct Fixture {
    pub(crate) m: Mollusk,
    pub(crate) a: Accounts,
    pub(crate) keys: [Pubkey; 19],
    pub(crate) slot: u64,
    pub(crate) time: u64,
    manifest: Value,
    rows: Vec<Value>,
}
impl Fixture {
    pub(crate) fn load(root: PathBuf) -> Self {
        let dir = root.join("artifacts/snapshots");
        let manifest: Value =
            serde_json::from_slice(&fs::read(dir.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(
            manifest["genesis_hash"],
            "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d"
        );
        let mut a = Vec::new();
        for row in manifest["accounts"].as_array().unwrap() {
            let data = fs::read(dir.join(row["file"].as_str().unwrap())).unwrap();
            assert_eq!(
                format!("{:x}", Sha256::digest(&data)),
                row["sha256"].as_str().unwrap()
            );
            let key = pk(row["pubkey"].as_str().unwrap());
            a.push((
                key,
                Account {
                    lamports: row["lamports"].as_u64().unwrap(),
                    data,
                    owner: pk(row["owner"].as_str().unwrap()),
                    executable: row["executable"].as_bool().unwrap(),
                    rent_epoch: row["rent_epoch"].as_u64().unwrap(),
                },
            ));
        }
        std::env::set_var("SBF_OUT_DIR", root.join("artifacts/sbf"));
        let mut m = Mollusk::new(&SETTLE, "stocklana_settle");
        for (name, id) in [
            ("phoenix", PHOENIX),
            ("raydium_cpmm", RAYDIUM),
            ("spl_token", TOKEN),
        ] {
            let elf = fs::read(root.join(format!("artifacts/sbf/{name}.so"))).unwrap();
            assert_eq!(
                format!("{:x}", Sha256::digest(&elf)),
                manifest["programs"][name]["elf_sha256"].as_str().unwrap()
            );
            m.add_program(&id, name);
        }
        let clock = get(&a, &pubkey!("SysvarC1ock11111111111111111111111111111111"));
        let slot = dex::u64_at(&clock.data, 0).unwrap();
        let time = dex::u64_at(&clock.data, 32).unwrap();
        assert_eq!(slot, manifest["context"]["slot"].as_u64().unwrap());
        m.sysvars.clock.slot = slot;
        m.sysvars.clock.epoch = dex::u64_at(&clock.data, 16).unwrap();
        m.sysvars.clock.unix_timestamp = time as i64;
        m.compute_budget.compute_unit_limit = CU_LIMIT;
        let market = pk(manifest["phoenix_market"].as_str().unwrap());
        let pool = pk(manifest["raydium_pool"].as_str().unwrap());
        let ph = PhoenixHeader::decode(&get(&a, &market).data).unwrap();
        let cp = RaydiumPool::decode(&get(&a, &pool).data, time).unwrap();
        assert_eq!(cp.mints, [ph.base_mint, ph.quote_mint]);
        let (nonce, _) = Pubkey::find_program_address(&[b"stocklana", WALLET.as_ref()], &SETTLE);
        let keys = [
            WALLET,
            nonce,
            USER_BASE,
            USER_QUOTE,
            TOKEN,
            Pubkey::from(ph.base_mint),
            Pubkey::from(ph.quote_mint),
            PHOENIX,
            pubkey!("7aDTsspkQNGKmrexAN7FLx9oxU3iPczSSvHNggyuqYkR"),
            market,
            Pubkey::from(ph.base_vault),
            Pubkey::from(ph.quote_vault),
            RAYDIUM,
            pubkey!("GpMZbSM2GgvTKHJirzeGfMFoaZ8UR2X7F4v8vHTvxFbL"),
            Pubkey::from(cp.config),
            pool,
            Pubkey::from(cp.vaults[0]),
            Pubkey::from(cp.vaults[1]),
            Pubkey::from(cp.observation),
        ];
        a.push((
            WALLET,
            Account {
                lamports: 10_000_000_000,
                ..Account::default()
            },
        ));
        a.push((USER_BASE, token(keys[5])));
        a.push((USER_QUOTE, token(keys[6])));
        a.push((nonce, Account::default()));
        a.push((
            Pubkey::default(),
            Account {
                lamports: 1,
                owner: pubkey!("NativeLoader1111111111111111111111111111111"),
                executable: true,
                ..Account::default()
            },
        ));
        a.push((
            SETTLE,
            mollusk_svm::program::create_program_account_loader_v3(&SETTLE),
        ));
        let ix = Instruction {
            program_id: SETTLE,
            accounts: vec![
                AccountMeta::new(WALLET, true),
                AccountMeta::new(nonce, false),
                AccountMeta::new_readonly(Pubkey::default(), false),
            ],
            data: vec![0],
        };
        let init = m.process_transaction_instructions(&[ix], &a);
        assert!(
            init.program_result.is_ok(),
            "SBF nonce init: {:?}",
            init.program_result
        );
        assert_eq!(
            &get(&init.resulting_accounts, &nonce).data[..8],
            b"SKEWSEQ1"
        );
        let a = init.resulting_accounts;
        Self {
            m,
            a,
            keys,
            slot,
            time,
            manifest,
            rows: vec![json!({"case":"nonce_init","cu":init.compute_units_consumed,"status":"ok"})],
        }
    }
    fn metas(&self, indices: &[usize], writable: &[usize], signer: usize) -> Vec<AccountMeta> {
        indices
            .iter()
            .enumerate()
            .map(|(i, j)| {
                if writable.contains(&i) {
                    AccountMeta::new(self.keys[*j], i == signer)
                } else {
                    AccountMeta::new_readonly(self.keys[*j], i == signer)
                }
            })
            .collect()
    }
    pub(crate) fn ray(&self, sell: bool, input: u64, min: u64) -> Instruction {
        let indices = if sell {
            [0, 13, 14, 15, 2, 3, 16, 17, 4, 4, 5, 6, 18]
        } else {
            [0, 13, 14, 15, 3, 2, 17, 16, 4, 4, 6, 5, 18]
        };
        Instruction {
            program_id: RAYDIUM,
            accounts: self.metas(&indices, &[0, 3, 4, 5, 6, 7, 12], 0),
            data: dex::raydium_swap(input, min).to_vec(),
        }
    }
    pub(crate) fn phoenix(&self, sell: bool, input: u64, matches: u64) -> Instruction {
        let header = PhoenixHeader::decode(&get(&self.a, &self.keys[9]).data).unwrap();
        let data = dex::phoenix_ioc(&header, sell, input, matches, self.slot + 100).unwrap();
        Instruction {
            program_id: PHOENIX,
            accounts: self.metas(&[7, 8, 9, 0, 2, 3, 10, 11, 4], &[2, 4, 5, 6, 7], 3),
            data: data[..dex::PHOENIX_IOC_LEN].to_vec(),
        }
    }
    fn settle(
        &self,
        mode: u8,
        sell: bool,
        input: u64,
        min: u64,
        budget: u64,
        matches: u8,
    ) -> Instruction {
        let mut data = vec![1, mode, u8::from(sell), matches];
        for v in [0, input, min, self.slot + 100, budget] {
            data.extend_from_slice(&v.to_le_bytes());
        }
        Instruction {
            program_id: SETTLE,
            accounts: self.metas(
                &(0..19).collect::<Vec<_>>(),
                &[0, 1, 2, 3, 9, 10, 11, 15, 16, 17, 18],
                0,
            ),
            data,
        }
    }
    fn run(&self, ix: &Instruction, a: &Accounts) -> TransactionResult {
        self.m
            .process_transaction_instructions(std::slice::from_ref(ix), a)
    }
    fn trace(r: &TransactionResult) -> Vec<String> {
        let keys = r.message.as_ref().unwrap().account_keys();
        r.inner_instructions
            .iter()
            .flatten()
            .map(|inner| keys[inner.instruction.program_id_index as usize].to_string())
            .collect()
    }
    fn deltas(&self, r: &TransactionResult, sell: bool) -> (u64, u64) {
        let (src, dst) = if sell {
            (USER_BASE, USER_QUOTE)
        } else {
            (USER_QUOTE, USER_BASE)
        };
        (
            START - amount(&r.resulting_accounts, &src),
            amount(&r.resulting_accounts, &dst) - START,
        )
    }
    fn record(
        &mut self,
        name: &str,
        r: &TransactionResult,
        sell: bool,
        input: u64,
        provenance: &str,
    ) {
        let (spent, out) = self.deltas(r, sell);
        let row = json!({"case":name,"sell_base":sell,"input":input,"spent":spent,"output":out,
            "cu":r.compute_units_consumed,"status":format!("{:?}",r.program_result),"provenance":provenance,"return_data":r.return_data,"cpi_programs":Self::trace(r)});
        println!("{row}");
        self.rows.push(row);
    }
    fn reject(&mut self, name: &str, ix: Instruction, a: Accounts, custom: Option<u32>) {
        let r = self.run(&ix, &a);
        assert!(r.program_result.is_err(), "{name} unexpectedly succeeded");
        if let Some(code) = custom {
            assert!(
                format!("{:?}", r.program_result).contains(&format!("Custom({code})")),
                "{name}: {:?}",
                r.program_result
            );
        }
        assert_eq!(r.resulting_accounts, a, "{name}: atomic state rollback");
        let trace = Self::trace(&r);
        if ["min_out_after_both_cpis", "fatal_raydium_cpi_after_phoenix"].contains(&name) {
            let ph = trace
                .iter()
                .position(|id| id == &PHOENIX.to_string())
                .unwrap();
            let ray = trace
                .iter()
                .position(|id| id == &RAYDIUM.to_string())
                .unwrap();
            assert!(ph < ray, "must actually reach Raydium after Phoenix");
        }
        self.rows.push(json!({"case":name,"status":format!("{:?}",r.program_result),"cu":r.compute_units_consumed,"all_accounts_unchanged":true,"provenance":"explicit_fault_injection","cpi_programs":trace}));
    }
    fn verify(&mut self) {
        let base = [
            1_000_000,
            10_000_000,
            100_000_000,
            1_000_000_000,
            10_000_000_000,
            100_000_000_000,
        ];
        for sell in [false, true] {
            for input in base {
                let cp =
                    RaydiumPool::decode(&get(&self.a, &self.keys[15]).data, self.time).unwrap();
                let expected = cp
                    .quote(
                        &get(&self.a, &self.keys[14]).data,
                        [
                            amount(&self.a, &self.keys[16]),
                            amount(&self.a, &self.keys[17]),
                        ],
                        sell,
                        input,
                    )
                    .unwrap();
                let direct = self.run(&self.ray(sell, input, expected), &self.a);
                assert!(
                    direct.program_result.is_ok(),
                    "Raydium direct: {:?}",
                    direct.program_result
                );
                assert_eq!(self.deltas(&direct, sell), (input, expected));
                self.record(
                    "raydium_direct",
                    &direct,
                    sell,
                    input,
                    "captured_dex_state_synthetic_wallet",
                );
                let wrapped = self.run(&self.settle(2, sell, input, expected, 0, 16), &self.a);
                assert!(
                    wrapped.program_result.is_ok(),
                    "Raydium settlement: {:?}",
                    wrapped.program_result
                );
                assert_eq!(self.deltas(&wrapped, sell), (input, expected));
                assert_eq!(
                    get(&wrapped.resulting_accounts, &self.keys[15]),
                    get(&direct.resulting_accounts, &self.keys[15])
                );
                self.record(
                    "raydium_settlement",
                    &wrapped,
                    sell,
                    input,
                    "captured_dex_state_synthetic_wallet",
                );
            }
            for matches in [1u8, 4, 16] {
                let input = 1_000_000_000;
                let direct = self.run(&self.phoenix(sell, input, u64::from(matches)), &self.a);
                assert!(
                    direct.program_result.is_ok(),
                    "Phoenix direct: {:?}",
                    direct.program_result
                );
                let (spent, first_out) = self.deltas(&direct, sell);
                let quote = PhoenixBook::decode(&get(&self.a, &self.keys[9]).data)
                    .unwrap()
                    .quote(
                        &WALLET.to_bytes(),
                        sell,
                        input,
                        u64::from(matches),
                        self.slot,
                        self.time,
                    )
                    .unwrap();
                assert_eq!(
                    (spent, first_out),
                    (quote.input, quote.output),
                    "Phoenix exact integer quote"
                );
                assert!(spent <= input);
                assert!(first_out > 0, "captured book has no executable fill");
                self.record(
                    "phoenix_direct",
                    &direct,
                    sell,
                    input,
                    "captured_dex_state_synthetic_wallet",
                );
                let residual = input - spent;
                let reference = if residual > 0 {
                    self.run(&self.ray(sell, residual, 1), &direct.resulting_accounts)
                } else {
                    direct.clone()
                };
                assert!(
                    reference.program_result.is_ok(),
                    "direct residual: {:?}",
                    reference.program_result
                );
                let expected = self.deltas(&reference, sell).1;
                let ix = self.settle(3, sell, input, expected, input, matches);
                let wrapped = self.run(&ix, &self.a);
                assert!(
                    wrapped.program_result.is_ok(),
                    "reflow settlement: {:?}",
                    wrapped.program_result
                );
                assert_eq!(self.deltas(&wrapped, sell), (input, expected));
                assert_eq!(dex::u64_at(&wrapped.return_data, 24).unwrap(), spent);
                for index in [2, 3, 9, 10, 11, 15, 16, 17, 18] {
                    assert_eq!(
                        get(&wrapped.resulting_accounts, &self.keys[index]),
                        get(&reference.resulting_accounts, &self.keys[index]),
                        "CPI conformance account {index}"
                    );
                }
                self.record(
                    "atomic_reflow",
                    &wrapped,
                    sell,
                    input,
                    "captured_dex_state_synthetic_wallet",
                );
                self.reject("nonce_replay", ix, wrapped.resulting_accounts, Some(2));
            }
        }
        let good = self.settle(3, true, 1_000_000_000, 1, 500_000_000, 1);
        let mut bad = good.clone();
        bad.data[20..28].copy_from_slice(&u64::MAX.to_le_bytes());
        self.reject("min_out_after_both_cpis", bad, self.a.clone(), Some(5));
        let mut bad = good.clone();
        bad.data[28..36].copy_from_slice(&(self.slot - 1).to_le_bytes());
        self.reject("expired", bad, self.a.clone(), Some(3));
        let mut bad = good.clone();
        bad.accounts[0].is_signer = false;
        self.reject("missing_signer", bad, self.a.clone(), Some(1));
        let mut bad = good.clone();
        bad.data[3] = 17;
        self.reject("unbounded_matches", bad, self.a.clone(), Some(4));
        let mut bad = good.clone();
        bad.accounts[3].pubkey = bad.accounts[2].pubkey;
        self.reject("token_alias", bad, self.a.clone(), Some(1));
        let mut a = self.a.clone();
        get_mut(&mut a, &USER_QUOTE).data[32..64]
            .copy_from_slice(Pubkey::new_from_array([99; 32]).as_ref());
        self.reject("wrong_recipient", good.clone(), a, Some(1));
        let mut a = self.a.clone();
        get_mut(&mut a, &self.keys[15]).data[329] |= 4;
        self.reject("disabled_residual_after_phoenix", good.clone(), a, Some(7));
        // Valid pool layout but invalid observation account ownership: let the
        // actual Raydium CPI fail after a successful Phoenix fill.
        let mut a = self.a.clone();
        get_mut(&mut a, &self.keys[18]).owner = Pubkey::default();
        self.reject("fatal_raydium_cpi_after_phoenix", good.clone(), a, None);
        let mut a = self.a.clone();
        get_mut(&mut a, &self.keys[10]).owner = Pubkey::default();
        self.reject("wrong_vault_program", good, a, Some(1));
        // Creator fee modes and rounding boundaries on the deployed Raydium
        // binary. Mutations are fixture scenarios, never claimed as live trades.
        for mode in 0..3u8 {
            for (trade, creator) in [(2500u64, 700u64), (0, 1), (1, 9999)] {
                for sell in [false, true] {
                    for input in [10_001u64, 999_999, 1_000_001, 1_000_000_001] {
                        let mut a = self.a.clone();
                        get_mut(&mut a, &self.keys[15]).data[389] = mode;
                        get_mut(&mut a, &self.keys[15]).data[390] = 1;
                        get_mut(&mut a, &self.keys[14]).data[12..20]
                            .copy_from_slice(&trade.to_le_bytes());
                        get_mut(&mut a, &self.keys[14]).data[108..116]
                            .copy_from_slice(&creator.to_le_bytes());
                        let cp =
                            RaydiumPool::decode(&get(&a, &self.keys[15]).data, self.time).unwrap();
                        let expected = cp
                            .quote(
                                &get(&a, &self.keys[14]).data,
                                [amount(&a, &self.keys[16]), amount(&a, &self.keys[17])],
                                sell,
                                input,
                            )
                            .unwrap();
                        let r = self.run(&self.settle(2, sell, input, expected, 0, 16), &a);
                        assert!(r.program_result.is_ok(),"creator mode {mode}, rates {trade}/{creator}, sell {sell}, input {input}: {:?}",r.program_result);
                        assert_eq!(self.deltas(&r, sell), (input, expected));
                        self.record(
                            "raydium_creator_fee_rounding",
                            &r,
                            sell,
                            input,
                            "mutated_fee_fixture_real_deployed_elf",
                        );
                    }
                }
            }
        }
        // Test more than one price level and exact lot boundaries on both sides.
        for sell in [false, true] {
            for input in [
                999_999u64,
                1_000_001,
                99_999_999,
                10_000_000_001,
                500_000_000_000,
            ] {
                for matches in [1u8, 4, 16] {
                    let q = PhoenixBook::decode(&get(&self.a, &self.keys[9]).data)
                        .unwrap()
                        .quote(
                            &WALLET.to_bytes(),
                            sell,
                            input,
                            u64::from(matches),
                            self.slot,
                            self.time,
                        )
                        .unwrap();
                    if input < if sell { 1_000_000 } else { 1 } {
                        continue;
                    }
                    let direct = self.run(&self.phoenix(sell, input, u64::from(matches)), &self.a);
                    assert!(
                        direct.program_result.is_ok(),
                        "Phoenix boundary: {:?}",
                        direct.program_result
                    );
                    assert_eq!(self.deltas(&direct, sell), (q.input, q.output));
                    self.record(
                        "phoenix_lot_and_depth_boundary",
                        &direct,
                        sell,
                        input,
                        "captured_dex_state_synthetic_wallet",
                    );
                }
            }
        }
        // Same market, future clock: encountered expired makers count toward
        // match limits; zero fills must preserve the residual for Raydium.
        let future = self.time + 86_400;
        for sell in [false, true] {
            let mut a = self.a.clone();
            let clock = pubkey!("SysvarC1ock11111111111111111111111111111111");
            get_mut(&mut a, &clock).data[32..40].copy_from_slice(&future.to_le_bytes());
            let market = &mut get_mut(&mut a, &self.keys[9]).data;
            let bids = dex::u64_at(market, 16).unwrap() as usize;
            let tree = if sell { 880 } else { 880 + 32 + bids * 64 };
            let read32 = |bytes: &[u8], o: usize| {
                u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap()) as usize
            };
            let mut index = read32(market, tree);
            for _ in 0..32 {
                let left = read32(market, tree + 32 + (index - 1) * 64);
                if left == 0 {
                    break;
                }
                index = left;
            }
            let offset = tree + 32 + (index - 1) * 64 + 48;
            market[offset..offset + 8].copy_from_slice(&(self.slot - 1).to_le_bytes());
            let q = PhoenixBook::decode(&get(&a, &self.keys[9]).data)
                .unwrap()
                .quote(
                    &WALLET.to_bytes(),
                    sell,
                    1_000_000_000,
                    16,
                    self.slot,
                    future,
                )
                .unwrap();
            let r = self.run(&self.phoenix(sell, 1_000_000_000, 16), &a);
            assert!(r.program_result.is_ok());
            assert_eq!(self.deltas(&r, sell), (q.input, q.output));
            self.record(
                "phoenix_expired_makers",
                &r,
                sell,
                1_000_000_000,
                "mutated_clock_and_best_maker_expiry_real_deployed_elf",
            );
        }
        let mut a = self.a.clone();
        let market = &mut get_mut(&mut a, &self.keys[9]).data;
        let bids = dex::u64_at(market, 16).unwrap() as usize;
        let traders = 880 + 64 + 128 * bids;
        let root = u32::from_le_bytes(market[traders..traders + 4].try_into().unwrap()) as usize;
        let node = traders + 32 + (root - 1) * 144;
        market[node + 16..node + 48].copy_from_slice(WALLET.as_ref());
        market[node + 56..node + 64].copy_from_slice(&1u64.to_le_bytes());
        market[node + 72..node + 80].copy_from_slice(&1u64.to_le_bytes());
        self.reject(
            "deposited_input_not_in_wallet_budget",
            self.settle(3, true, 1_000_000_000, 1, 500_000_000, 1),
            a,
            Some(7),
        );
        let mut a = self.a.clone();
        get_mut(&mut a, &self.keys[1]).data[40..48].copy_from_slice(&u64::MAX.to_le_bytes());
        let mut ix = self.settle(2, true, 1_000_000_000, 1, 0, 1);
        ix.data[4..12].copy_from_slice(&u64::MAX.to_le_bytes());
        self.reject("nonce_overflow", ix, a, Some(4));
        // Unsigned caller cannot pick an arbitrary token/CPI program.
        let mut ix = self.settle(2, true, 1_000_000_000, 1, 0, 1);
        ix.accounts[12].pubkey = TOKEN;
        self.reject("arbitrary_cpi_program", ix, self.a.clone(), Some(1));
        let mut ix = self.settle(2, true, 1_000_000_000, 1, 0, 1);
        ix.data[4..12].copy_from_slice(&u64::MAX.to_le_bytes());
        self.reject("nonce_not_current", ix, self.a.clone(), Some(2));
        for input in [
            100_000_000u64,
            1_000_000_000,
            10_000_000_000,
            50_000_000_000,
            100_000_000_000,
            500_000_000_000,
        ] {
            for matches in [1u8, 4, 16] {
                let q = PhoenixBook::decode(&get(&self.a, &self.keys[9]).data)
                    .unwrap()
                    .quote(
                        &WALLET.to_bytes(),
                        false,
                        input,
                        u64::from(matches),
                        self.slot,
                        self.time,
                    )
                    .unwrap();
                let direct = self.run(&self.phoenix(false, input, u64::from(matches)), &self.a);
                assert!(direct.program_result.is_ok());
                assert_eq!(self.deltas(&direct, false), (q.input, q.output));
                let residual = input - q.input;
                let reference = if residual > 0 {
                    self.run(&self.ray(false, residual, 1), &direct.resulting_accounts)
                } else {
                    direct
                };
                assert!(reference.program_result.is_ok());
                let expected = self.deltas(&reference, false).1;
                let r = self.run(
                    &self.settle(3, false, input, expected, input, matches),
                    &self.a,
                );
                assert!(
                    r.program_result.is_ok(),
                    "size matrix {input}/{matches}: {:?}",
                    r.program_result
                );
                assert_eq!(self.deltas(&r, false), (input, expected));
                self.record(
                    "usdc_size_matrix",
                    &r,
                    false,
                    input,
                    "captured_sol_usdc_state_synthetic_wallet",
                );
            }
        }
        // A sub-lot allocation is skipped and must not invent an execution leg.
        let r = self.run(&self.settle(3, true, 1_000_000_000, 1, 1, 16), &self.a);
        assert!(r.program_result.is_ok());
        assert_eq!(dex::u64_at(&r.return_data, 24).unwrap(), 0);
        assert_eq!(dex::u64_at(&r.return_data, 32).unwrap(), 1);
        assert!(!Self::trace(&r).contains(&PHOENIX.to_string()));
        self.record(
            "sub_lot_skips_phoenix",
            &r,
            true,
            1_000_000_000,
            "captured_dex_state_synthetic_wallet",
        );

        let mut a = self.a.clone();
        let market = &mut get_mut(&mut a, &self.keys[9]).data;
        let capacity = dex::u64_at(market, 16).unwrap() as usize;
        for index in 0..capacity {
            let offset = 880 + 32 + index * 64 + 48;
            market[offset..offset + 8].copy_from_slice(&(self.slot - 1).to_le_bytes());
        }
        let q = PhoenixBook::decode(market)
            .unwrap()
            .quote(
                &WALLET.to_bytes(),
                true,
                500_000_000,
                16,
                self.slot,
                self.time,
            )
            .unwrap();
        assert_eq!((q.input, q.output, q.encountered), (0, 0, 16));
        let r = self.run(&self.settle(3, true, 1_000_000_000, 1, 500_000_000, 16), &a);
        assert!(r.program_result.is_ok());
        assert_eq!(dex::u64_at(&r.return_data, 32).unwrap(), 1);
        assert_eq!(
            get(&r.resulting_accounts, &self.keys[9]),
            get(&a, &self.keys[9])
        );
        assert!(!Self::trace(&r).contains(&PHOENIX.to_string()));
        self.record(
            "expired_prefix_skips_zero_output_cpi",
            &r,
            true,
            1_000_000_000,
            "all_bid_expiries_mutated_real_deployed_elf",
        );

        let mut collision = USER_QUOTE.to_bytes();
        collision[..8].copy_from_slice(&USER_BASE.to_bytes()[..8]);
        let collision = Pubkey::from(collision);
        let mut a = self.a.clone();
        a.push((collision, get(&a, &USER_QUOTE).clone()));
        let mut ix = self.settle(2, true, 1_000_000_000, 1, 0, 1);
        ix.accounts[3].pubkey = collision;
        let r = self.run(&ix, &a);
        assert!(r.program_result.is_ok());
        assert!(amount(&r.resulting_accounts, &collision) > START);
        self.rows.push(json!({"case":"benign_key_prefix_collision","cu":r.compute_units_consumed,"status":"Success","full_key_comparison":true}));

        let init = Instruction {
            program_id: SETTLE,
            accounts: vec![
                AccountMeta::new(WALLET, true),
                AccountMeta::new(self.keys[1], false),
                AccountMeta::new_readonly(Pubkey::default(), false),
            ],
            data: vec![0],
        };
        self.reject("nonce_reinitialization", init, self.a.clone(), Some(1));
        self.m.compute_budget.compute_unit_limit = 7000;
        let ix = self.settle(3, true, 1_000_000_000, 1, 500_000_000, 16);
        let r = self.run(&ix, &self.a);
        assert!(r.program_result.is_err());
        assert_eq!(r.resulting_accounts, self.a);
        assert_eq!(
            r.compute_units_consumed, 7000,
            "budget exhaustion: {:?}",
            r.program_result
        );
        self.rows.push(json!({"case":"compute_exhaustion","cu":r.compute_units_consumed,"budget":7000,"status":format!("{:?}",r.program_result),"all_accounts_unchanged":true}));
        self.m.compute_budget.compute_unit_limit = CU_LIMIT;
    }
}
fn main() {
    if std::env::args().nth(1).as_deref() == Some("--derive-nonce") {
        let wallet = pk(&std::env::args().nth(2).expect("wallet public key"));
        let (nonce, bump) = Pubkey::find_program_address(&[b"stocklana", wallet.as_ref()], &SETTLE);
        println!(
            "{}",
            json!({"nonce":nonce.to_string(),"bump":bump,"program_id":SETTLE.to_string()})
        );
        return;
    }
    let root = PathBuf::from(
        std::env::args()
            .skip(1)
            .find(|x| !x.starts_with("--"))
            .unwrap_or_else(|| ".".into()),
    )
    .canonicalize()
    .unwrap();
    let mut f = Fixture::load(root.clone());
    f.verify();
    let mut budget = vec![2u8];
    budget.extend_from_slice(&(CU_LIMIT as u32).to_le_bytes());
    let message = solana_message::Message::new(
        &[
            Instruction {
                program_id: pubkey!("ComputeBudget111111111111111111111111111111"),
                accounts: vec![],
                data: budget,
            },
            f.settle(3, false, 50_000_000_000, 1, 50_000_000_000, 16),
        ],
        Some(&WALLET),
    );
    let message_bytes = message.serialize();
    let transaction_bytes =
        1 + usize::from(message.header.num_required_signatures) * 64 + message_bytes.len();
    assert_eq!(message.header.num_required_signatures, 1);
    assert!(transaction_bytes <= 1232);
    let evidence = json!({"scope":"SBF execution in Mollusk; no network submission, signature verification, or landing claim",
        "snapshot":f.manifest,"settlement_elf_sha256":format!("{:x}",Sha256::digest(fs::read(root.join("artifacts/sbf/stocklana_settle.so")).unwrap())),
        "compute_limit":CU_LIMIT,"transaction_wire_check":{"bytes":transaction_bytes,"max_bytes":1232,"account_keys":message.account_keys.len(),"required_signatures":message.header.num_required_signatures,"includes_compute_budget":true,"signed":false,"blockhash":"placeholder"},"rows":f.rows});
    fs::write(
        root.join(if std::env::args().any(|x| x == "--fair-execution") {
            "artifacts/fair-execution/svm-proof.json"
        } else if std::env::args().any(|x| x == "--seven-gates") {
            "artifacts/seven-gates/svm-proof.json"
        } else {
            "artifacts/evidence/svm-proof.json"
        }),
        serde_json::to_vec_pretty(&evidence).unwrap(),
    )
    .unwrap();
    println!(
        "VERIFIED {} SBF rows",
        evidence["rows"].as_array().unwrap().len()
    );
}
