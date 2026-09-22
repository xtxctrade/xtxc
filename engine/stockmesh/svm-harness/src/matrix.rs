//! Equal-state deployed-program comparison: Jupiter -> direct typed adapter ->
//! Skew SBF graph. Captured states, synthetic wallet balances, no submission.
use base64::{engine::general_purpose::STANDARD, Engine};
use mollusk_svm::{result::types::TransactionResult, Mollusk};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use solana_account::Account;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::{pubkey, Pubkey};
use std::{
    fs,
    path::{Path, PathBuf},
    str::FromStr,
};
use stocklana_adapters::{self as dex, graph::Venue};

#[allow(dead_code)]
#[path = "../../host/src/swap_wire.rs"]
mod swap_wire;

pub(crate) const WALLET: Pubkey = Pubkey::new_from_array([71; 32]);
pub(crate) const SETTLE: Pubkey = Pubkey::new_from_array([83; 32]);
const TOKEN: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const TOKEN22: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
const MEMO: Pubkey = pubkey!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");
const CLOCK: Pubkey = pubkey!("SysvarC1ock11111111111111111111111111111111");
pub(crate) const START: u64 = 100_000_000_000_000;
pub(crate) type Accounts = Vec<(Pubkey, Account)>;
pub(crate) fn pk(s: &str) -> Pubkey {
    Pubkey::from_str(s).unwrap()
}
pub(crate) fn read(p: &Path) -> Value {
    serde_json::from_slice(&fs::read(p).unwrap()).unwrap()
}
pub(crate) fn get<'a>(a: &'a Accounts, k: &Pubkey) -> &'a Account {
    &a.iter()
        .find(|x| &x.0 == k)
        .unwrap_or_else(|| panic!("missing {k}"))
        .1
}
pub(crate) fn put(a: &mut Accounts, k: Pubkey, v: Account) {
    if let Some(x) = a.iter_mut().find(|x| x.0 == k) {
        x.1 = v;
    } else {
        a.push((k, v));
    }
}
pub(crate) fn amount(a: &Accounts, k: &Pubkey) -> u64 {
    dex::u64_at(&get(a, k).data, 64).unwrap()
}
pub(crate) fn token(mint: Pubkey, owner: Pubkey, md: &[u8]) -> Account {
    let mut d = vec![0u8; if owner == TOKEN22 { 166 } else { 165 }];
    d[..32].copy_from_slice(mint.as_ref());
    d[32..64].copy_from_slice(WALLET.as_ref());
    d[64..72].copy_from_slice(&START.to_le_bytes());
    d[108] = 1;
    if owner == TOKEN22 {
        d[165] = 2;
        d.extend_from_slice(&[7, 0, 0, 0]);
        let mut p = 166;
        while p + 4 <= md.len() {
            let kind = u16::from_le_bytes([md[p], md[p + 1]]);
            let len = u16::from_le_bytes([md[p + 2], md[p + 3]]) as usize;
            let account_extension = match kind {
                1 => Some((2u16, 8u16)),
                14 => Some((15, 1)),
                26 => Some((27, 0)),
                _ => None,
            };
            if let Some((kind, len)) = account_extension {
                d.extend_from_slice(&kind.to_le_bytes());
                d.extend_from_slice(&len.to_le_bytes());
                d.resize(d.len() + len as usize, 0);
            }
            p += 4 + len;
        }
    }
    let mut lamports = 10_000_000;
    if mint == pubkey!("So11111111111111111111111111111111111111112") {
        d[109..113].copy_from_slice(&1u32.to_le_bytes());
        d[113..121].copy_from_slice(&2_039_280u64.to_le_bytes());
        lamports = START + 2_039_280;
    }
    Account {
        lamports,
        data: d,
        owner,
        executable: false,
        rent_epoch: 0,
    }
}
pub(crate) fn instruction(v: &Value) -> Instruction {
    Instruction {
        program_id: pk(v["programId"].as_str().unwrap()),
        data: STANDARD.decode(v["data"].as_str().unwrap()).unwrap(),
        accounts: v["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| AccountMeta {
                pubkey: pk(x["pubkey"].as_str().unwrap()),
                is_signer: x["isSigner"].as_bool().unwrap(),
                is_writable: x["isWritable"].as_bool().unwrap(),
            })
            .collect(),
    }
}
fn delta(r: &TransactionResult, src: &Pubkey, dst: &Pubkey) -> (u64, u64) {
    (
        START
            .checked_sub(amount(&r.resulting_accounts, src))
            .unwrap(),
        amount(&r.resulting_accounts, dst)
            .checked_sub(START)
            .unwrap(),
    )
}
pub(crate) fn run(m: &Mollusk, ix: &Instruction, a: &Accounts) -> TransactionResult {
    m.process_transaction_instructions(std::slice::from_ref(ix), a)
}

pub(crate) struct Case {
    pub(crate) name: String,
    pub(crate) a: Accounts,
    pub(crate) venue: Venue,
    pub(crate) quote: Value,
    pub(crate) jup: Instruction,
    pub(crate) input: Pubkey,
    pub(crate) output: Pubkey,
    pub(crate) slot: u64,
    pub(crate) time: u64,
}
impl Case {
    pub(crate) fn load(path: &Path) -> Self {
        let v = read(&path.join("manifest.json"));
        let q = v["quote"].clone();
        let jup = instruction(&q["swapInstruction"]);
        let mut a = Vec::new();
        for row in v["accounts"].as_array().unwrap() {
            let data = fs::read(path.join(row["file"].as_str().unwrap())).unwrap();
            assert_eq!(
                format!("{:x}", Sha256::digest(&data)),
                row["sha256"].as_str().unwrap()
            );
            if row["pubkey"] == "Sysvar1nstructions1111111111111111111111111" {
                continue;
            }
            a.push((
                pk(row["pubkey"].as_str().unwrap()),
                Account {
                    lamports: row["lamports"].as_u64().unwrap(),
                    owner: pk(row["owner"].as_str().unwrap()),
                    executable: row["executable"].as_bool().unwrap(),
                    data,
                    rent_epoch: 0,
                },
            ));
        }
        put(
            &mut a,
            WALLET,
            Account {
                lamports: 1_000_000_000_000,
                ..Account::default()
            },
        );
        // Only create fixture accounts belonging to this fixture wallet.
        for setup in q["setupInstructions"].as_array().unwrap() {
            if setup["programId"] == "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL" {
                let keys = setup["accounts"].as_array().unwrap();
                assert_eq!(pk(keys[2]["pubkey"].as_str().unwrap()), WALLET);
                let mint = pk(keys[3]["pubkey"].as_str().unwrap());
                let owner = pk(keys[5]["pubkey"].as_str().unwrap());
                assert_eq!(get(&a, &mint).owner, owner);
                let account = token(mint, owner, &get(&a, &mint).data);
                put(&mut a, pk(keys[1]["pubkey"].as_str().unwrap()), account);
            }
        }
        let (nonce, _) = Pubkey::find_program_address(&[b"stocklana", WALLET.as_ref()], &SETTLE);
        let mut n = vec![0u8; 64];
        n[..8].copy_from_slice(b"SKEWSEQ1");
        n[8..40].copy_from_slice(WALLET.as_ref());
        put(
            &mut a,
            nonce,
            Account {
                lamports: 10_000_000,
                data: n,
                owner: SETTLE,
                executable: false,
                rent_epoch: 0,
            },
        );
        put(
            &mut a,
            SETTLE,
            mollusk_svm::program::create_program_account_loader_v3(&SETTLE),
        );
        let im = pk(q["inputMint"].as_str().unwrap());
        let om = pk(q["outputMint"].as_str().unwrap());
        for mint in [im, om] {
            let owner = get(&a, &mint).owner;
            let account = token(mint, owner, &get(&a, &mint).data);
            let (ata, _) = Pubkey::find_program_address(
                &[WALLET.as_ref(), owner.as_ref(), mint.as_ref()],
                &pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"),
            );
            assert!(
                jup.accounts.iter().any(|m| m.pubkey == ata),
                "expected fixture ATA {ata}"
            );
            put(&mut a, ata, account);
        }
        // Canonical wallet accounts for intermediate assets, without mutating any
        // Jupiter-owned account used by the reference execution.
        let mints: Vec<_> = a
            .iter()
            .filter(|(_, v)| {
                (v.owner == TOKEN && v.data.len() == 82)
                    || (v.owner == TOKEN22 && v.data.len() >= 166 && v.data[165] == 1)
            })
            .map(|(k, _)| *k)
            .collect();
        for mint in mints {
            let owner = get(&a, &mint).owner;
            let (ata, _) = Pubkey::find_program_address(
                &[WALLET.as_ref(), owner.as_ref(), mint.as_ref()],
                &pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"),
            );
            let t = token(mint, owner, &get(&a, &mint).data);
            put(&mut a, ata, t);
        }
        let find_token = |mint: Pubkey| {
            Pubkey::find_program_address(
                &[
                    WALLET.as_ref(),
                    get(&a, &mint).owner.as_ref(),
                    mint.as_ref(),
                ],
                &pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"),
            )
            .0
        };
        let input = find_token(im);
        let output = find_token(om);
        let venue = (0u8..Venue::COUNT)
            .map(|x| Venue::try_from(x).unwrap())
            .find(|x| x.program() == v["venue_program"].as_str().unwrap())
            .unwrap();
        let clock = &get(&a, &CLOCK).data;
        let slot = dex::u64_at(clock, 0).unwrap();
        let time = dex::u64_at(clock, 32).unwrap();
        assert_eq!(slot, v["context"]["slot"].as_u64().unwrap());
        Self {
            name: path.file_name().unwrap().to_str().unwrap().to_string(),
            a,
            venue,
            quote: q,
            jup,
            input,
            output,
            slot,
            time,
        }
    }
    pub(crate) fn lower(&self, r: &TransactionResult) -> Result<Vec<(Instruction, bool)>, String> {
        let id = pk(self.venue.program());
        let msg = r.message.as_ref().unwrap();
        let keys = msg.account_keys();
        let matches: Vec<_> = r
            .inner_instructions
            .iter()
            .flatten()
            .filter(|x| {
                keys[x.instruction.program_id_index as usize] == id && x.stack_height == Some(2)
            })
            .collect();
        if matches.is_empty() || matches.len() > 4 {
            return Err(format!(
                "Bounded graph admits 1..4 venue CPIs, found {}",
                matches.len()
            ));
        }
        let mut lowered = Vec::new();
        for found in &matches {
            let raw = &found.instruction;
            let mut k: Vec<Pubkey> = raw.accounts.iter().map(|i| keys[*i as usize]).collect();
            if self.venue == Venue::RaydiumAmmV4 && k.len() >= 17 {
                let v = if k.len() == 18 { 5 } else { 4 };
                let n = k.len();
                k = vec![
                    k[0],
                    k[1],
                    k[2],
                    k[v],
                    k[v + 1],
                    k[n - 3],
                    k[n - 2],
                    k[n - 1],
                ];
            }
            if self.venue == Venue::OrcaWhirlpool && k.len() == 11 {
                let pool = &get(&self.a, &k[2]).data;
                let ma = Pubkey::from(dex::key(pool, 101).unwrap());
                let mb = Pubkey::from(dex::key(pool, 181).unwrap());
                k = vec![
                    get(&self.a, &ma).owner,
                    get(&self.a, &mb).owner,
                    MEMO,
                    k[1],
                    k[2],
                    ma,
                    mb,
                    k[3],
                    k[4],
                    k[5],
                    k[6],
                    k[7],
                    k[8],
                    k[9],
                    k[10],
                ];
            }
            if self.venue == Venue::MeteoraDlmm
                && raw.data.get(..8) == Some(&[248, 198, 158, 145, 225, 117, 135, 200])
            {
                k.insert(13, MEMO);
            }
            if matches!(self.venue, Venue::RaydiumClmm | Venue::ByrealClmm)
                && raw.data.get(..8) == Some(&[248, 198, 158, 145, 225, 117, 135, 200])
            {
                let im = Pubkey::from(dex::key(&get(&self.a, &k[5]).data, 0).unwrap());
                let om = Pubkey::from(dex::key(&get(&self.a, &k[6]).data, 0).unwrap());
                let mut v = k[..9].to_vec();
                v.extend_from_slice(&[TOKEN22, MEMO, im, om]);
                v.extend_from_slice(&k[9..]);
                k = v;
            }
            let (lo, hi) = self.venue.account_bounds();
            if k.len() < lo || k.len() > hi {
                return Err(format!(
                    "Unsupported CPI account count {}: {:?}; data={:?}",
                    k.len(),
                    k,
                    raw.data
                ));
            }
            let dir = match self.venue {
                Venue::Phoenix => k[4] == self.input,
                Venue::OrcaWhirlpool => raw.data[41] == 1,
                Venue::Riptide => raw.data[9] == 1,
                _ => true,
            };
            let (_, signer, src, dst) = self.venue.bindings(dir);
            k[signer] = WALLET;
            for position in [src, dst] {
                let t = get(&self.a, &k[position]);
                {
                    let mint =
                        Pubkey::from(dex::key(&t.data, 0).map_err(|_| "missing token mint")?);
                    let (ata, _) = Pubkey::find_program_address(
                        &[WALLET.as_ref(), t.owner.as_ref(), mint.as_ref()],
                        &pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"),
                    );
                    if !self.a.iter().any(|(key, _)| *key == ata) {
                        return Err("intermediate mint not captured".into());
                    }
                    k[position] = ata;
                }
            }
            let input = match self.venue {
                Venue::Phoenix => self.quote["inAmount"]
                    .as_str()
                    .unwrap()
                    .parse::<u64>()
                    .unwrap(),
                Venue::RaydiumAmmV4 | Venue::Riptide => dex::u64_at(&raw.data, 1).unwrap(),
                _ => dex::u64_at(&raw.data, 8).unwrap(),
            };
            let mut bytes = [0u8; 80];
            let len = if self.venue == Venue::Phoenix {
                let h = dex::PhoenixHeader::decode(&get(&self.a, &k[2]).data).unwrap();
                bytes = dex::phoenix_ioc(&h, dir, input, 16, self.slot + 1000).unwrap();
                dex::PHOENIX_IOC_LEN
            } else {
                self.venue.swap_data(input, dir, &mut bytes).unwrap()
            };
            let accounts = k
                .iter()
                .enumerate()
                .map(|(i, key)| AccountMeta {
                    pubkey: *key,
                    is_signer: i == signer,
                    is_writable: self.venue.writable_with_owner(
                        i,
                        self.venue != Venue::ByrealClmm || i < 13 || get(&self.a, key).owner == id,
                    ) && *key != id,
                })
                .collect();
            lowered.push((
                Instruction {
                    program_id: id,
                    accounts,
                    data: bytes[..len].to_vec(),
                },
                dir,
            ));
        }
        Ok(lowered)
    }
    pub(crate) fn graph(
        &self,
        direct: &[(Instruction, bool)],
        input: u64,
        min_out: u64,
    ) -> Result<Instruction, String> {
        self.graph_mixed(
            &direct
                .iter()
                .map(|(ix, dir)| (self.venue, ix.clone(), *dir))
                .collect::<Vec<_>>(),
            input,
            min_out,
        )
    }
    pub(crate) fn graph_mixed(
        &self,
        direct: &[(Venue, Instruction, bool)],
        input: u64,
        min_out: u64,
    ) -> Result<Instruction, String> {
        self.graph_compiled(direct, input, min_out, None)
    }

    #[allow(dead_code)] // Used by the dedicated reflow proof, not every harness binary.
    pub(crate) fn reflow(
        &self,
        candidates: &[(Venue, Instruction, bool)],
        input: u64,
        min_out: u64,
        calls: u8,
        seed: Option<u64>,
    ) -> Result<Instruction, String> {
        self.graph_compiled(candidates, input, min_out, Some((calls, seed)))
    }

    fn graph_compiled(
        &self,
        direct: &[(Venue, Instruction, bool)],
        input: u64,
        min_out: u64,
        reflow: Option<(u8, Option<u64>)>,
    ) -> Result<Instruction, String> {
        use swap_wire::{AccountView, Budget, SwapGraph, SwapLeg, TokenAsset};
        let mut edges = Vec::new();
        let mut tokens = vec![self.input];
        for (venue, ix, direction) in direct {
            let (_, _, src, dst) = venue.bindings(*direction);
            if ix.program_id != pk(venue.program()) {
                return Err("fixture typed program mismatch".into());
            }
            let source = ix.accounts.get(src).ok_or("fixture source")?.pubkey;
            let destination = ix.accounts.get(dst).ok_or("fixture destination")?.pubkey;
            edges.push((source, destination));
            for key in [source, destination] {
                if !tokens.contains(&key) {
                    tokens.push(key);
                }
            }
        }
        let asset = |token| -> Result<TokenAsset, String> {
            let a = get(&self.a, &token);
            Ok(TokenAsset {
                token,
                mint: Pubkey::from(dex::key(&a.data, 0).map_err(|_| "fixture token mint")?),
                token_program: a.owner,
            })
        };
        let legs = direct
            .iter()
            .zip(&edges)
            .enumerate()
            .map(|(i, ((venue, ix, direction), (source, destination)))| {
                let last_source = !edges[i + 1..].iter().any(|(s, _)| s == source);
                let budget = if reflow.is_some() || last_source {
                    Budget::Remaining
                } else {
                    // Fixture import only. Production receives exact typed native
                    // allocations, never arbitrary transaction instruction bytes.
                    Budget::Exact(match venue {
                        Venue::Phoenix => input,
                        Venue::RaydiumAmmV4 | Venue::Riptide => {
                            dex::u64_at(&ix.data, 1).map_err(|_| "fixture input")?
                        }
                        _ => dex::u64_at(&ix.data, 8).map_err(|_| "fixture input")?,
                    })
                };
                Ok(SwapLeg {
                    venue: *venue,
                    direction: *direction,
                    source: *source,
                    destination: *destination,
                    budget,
                    accounts: ix.accounts.iter().map(|meta| meta.pubkey).collect(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let spec = SwapGraph {
            owner: WALLET,
            sequence: 0,
            input_atoms: input,
            minimum_output_atoms: min_out,
            deadline_slot: self
                .slot
                .checked_add(1000)
                .ok_or("fixture deadline overflow")?,
            input: asset(self.input)?,
            output: asset(self.output)?,
            intermediates: tokens
                .iter()
                .filter(|key| **key != self.input && **key != self.output)
                .map(|key| asset(*key))
                .collect::<Result<Vec<_>, String>>()?,
            legs,
        };
        let read = |key: &Pubkey| {
            let a = self
                .a
                .iter()
                .find(|(k, _)| k == key)
                .ok_or_else(|| format!("fixture bank missing {key}"))?;
            Ok(AccountView {
                owner: a.1.owner,
                executable: a.1.executable,
                data: &a.1.data,
            })
        };
        match reflow {
            Some((calls, seed)) => {
                swap_wire::compile_reflow_graph(SETTLE, &spec, calls, seed, read)
            }
            None => swap_wire::compile_swap_graph(SETTLE, &spec, read),
        }
    }
}

pub(crate) fn runtime(root: &Path) -> Mollusk {
    let data = root.join("artifacts/venue-matrix");
    let programs = read(&data.join("programs.json"));
    let settle_dir = std::env::var_os("SKEW_SETTLE_SBF_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("artifacts/sbf"));
    std::env::set_var("SBF_OUT_DIR", settle_dir);
    let mut m = Mollusk::new(&SETTLE, "stocklana_settle");
    for p in programs.as_array().unwrap() {
        let path = data
            .join("programs")
            .join(format!("{}.so", p["name"].as_str().unwrap()));
        let bytes = fs::read(&path).unwrap();
        assert_eq!(
            format!("{:x}", Sha256::digest(&bytes)),
            p["elf_sha256"].as_str().unwrap()
        );
        m.add_program_with_loader_and_elf(
            &pk(p["id"].as_str().unwrap()),
            &pk(p["program_account"]["owner"].as_str().unwrap()),
            &bytes,
        );
    }
    // Match the production transaction ceiling.  A lower synthetic cap turns
    // otherwise valid Jupiter routes into false failures before their direct
    // and settlement legs can be checked.
    m.compute_budget.compute_unit_limit = 1_400_000;
    m
}
pub(crate) fn main() {
    let root = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "/srv/skew/stocklana-engine-20260912".into()),
    );
    // The Jupiter program is used only to discover the exact DEX CPI shape.
    // It is never included in the final wallet transaction.  Keep that
    // discovery replay separate from the final direct-DEX/SBF replay: Mollusk
    // charges the router's interpreter path far above mainnet transaction
    // limits, which must not be confused with the production wire budget.
    let mut reference = runtime(&root);
    reference.compute_budget.compute_unit_limit = 8_000_000;
    let mut m = runtime(&root);
    let data = root.join("artifacts/venue-matrix");
    let mut paths: Vec<_> = fs::read_dir(data.join("cases"))
        .unwrap()
        .map(|x| x.unwrap().path())
        .filter(|x| x.join("manifest.json").exists())
        .collect();
    paths.sort();
    let filter = std::env::args().nth(2).unwrap_or_default();
    let mut rows = Vec::new();
    for path in paths {
        if !path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .contains(&filter)
        {
            continue;
        }
        let c = Case::load(&path);
        let epoch = dex::u64_at(&get(&c.a, &CLOCK).data, 16).unwrap();
        for runtime in [&mut reference, &mut m] {
            runtime.sysvars.clock.slot = c.slot;
            runtime.sysvars.clock.unix_timestamp = c.time as i64;
            runtime.sysvars.clock.epoch = epoch;
        }
        let baseline = run(&reference, &c.jup, &c.a);
        let mut row = json!({"case":c.name,"slot":c.slot,"venue":c.venue.program(),"jupiter_reference_only":true,"production_compute_ceiling":1_400_000,"jupiter_status":format!("{:?}",baseline.program_result),"jupiter_cu":baseline.compute_units_consumed});
        if baseline.program_result.is_ok() {
            let (spent, out) = delta(&baseline, &c.input, &c.output);
            row["jupiter_spent"] = json!(spent);
            row["jupiter_output"] = json!(out);
            match c.lower(&baseline) {
                Ok(direct) => {
                    let direct_ix: Vec<_> = direct.iter().map(|(ix, _)| ix.clone()).collect();
                    let r = m.process_transaction_instructions(&direct_ix, &c.a);
                    row["direct_status"] = json!(format!("{:?}", r.program_result));
                    row["direct_cu"] = json!(r.compute_units_consumed);
                    row["legs"] = json!(direct.len());
                    if r.program_result.is_ok() {
                        let (spent, out) = delta(&r, &c.input, &c.output);
                        row["direct_spent"] = json!(spent);
                        row["direct_output"] = json!(out);
                        let input = c.quote["inAmount"]
                            .as_str()
                            .unwrap()
                            .parse::<u64>()
                            .unwrap();
                        let graph = match c.graph(&direct, input, out.max(1)) {
                            Ok(g) => g,
                            Err(e) => {
                                row["graph_error"] = json!(e);
                                println!("{row}");
                                rows.push(row);
                                continue;
                            }
                        };
                        let s = run(&m, &graph, &c.a);
                        row["skew_status"] = json!(format!("{:?}", s.program_result));
                        row["skew_cu"] = json!(s.compute_units_consumed);
                        row["return_data"] = json!(s.return_data);
                        if s.program_result.is_ok() {
                            assert_eq!(
                                delta(&s, &c.input, &c.output),
                                (input, out),
                                "typed SBF graph matches direct venue exactly"
                            );
                            row["skew_output"] = json!(out);
                            row["status"] = json!("DIRECT_AND_SBF_VERIFIED");
                            let mut bad = graph.clone();
                            bad.data[20..28].copy_from_slice(&(out + 1).to_le_bytes());
                            let fail = run(&m, &bad, &c.a);
                            assert!(fail.program_result.is_err());
                            assert_eq!(fail.resulting_accounts, c.a, "min-out atomic rollback");
                            row["min_out_rollback"] = json!(true);
                            let replay = run(&m, &graph, &s.resulting_accounts);
                            assert!(replay.program_result.is_err());
                            assert_eq!(replay.resulting_accounts, s.resulting_accounts);
                            row["nonce_replay_rejected"] = json!(true);
                            let compiled = json!({"kind":c.venue as u8,"input_account":c.input.to_string(),"output_account":c.output.to_string(),"direct":direct.iter().map(|(ix,dir)|json!({"direction":dir,"programId":ix.program_id.to_string(),"data":STANDARD.encode(&ix.data),"accounts":ix.accounts.iter().map(|a|json!({"pubkey":a.pubkey.to_string(),"isSigner":a.is_signer,"isWritable":a.is_writable})).collect::<Vec<_>>()})).collect::<Vec<_>>(),"graph":{"programId":SETTLE.to_string(),"data":STANDARD.encode(&graph.data),"accounts":graph.accounts.iter().map(|a|json!({"pubkey":a.pubkey.to_string(),"isSigner":a.is_signer,"isWritable":a.is_writable})).collect::<Vec<_>>()}});
                            fs::write(
                                path.join("compiled.json"),
                                serde_json::to_vec_pretty(&compiled).unwrap(),
                            )
                            .unwrap();
                        }
                    }
                }
                Err(e) => {
                    row["lowering_error"] = json!(e);
                }
            }
        }
        println!("{row}");
        rows.push(row);
        fs::write(data.join("proof.json"),serde_json::to_vec_pretty(&json!({"environment":"Mollusk 0.13.4; captured mainnet ELFs and state; synthetic balances","settlement_elf_sha256":format!("{:x}",Sha256::digest(fs::read(root.join("artifacts/sbf/stocklana_settle.so")).unwrap())),"rows":rows})).unwrap()).unwrap();
    }
}
