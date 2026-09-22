use super::*;
use std::collections::BTreeMap;

struct Row {
    owner: Pubkey,
    executable: bool,
    data: Vec<u8>,
}
struct Fixture {
    program: Pubkey,
    spec: SwapGraph,
    bank: BTreeMap<Pubkey, Row>,
}
fn key(n: u8) -> Pubkey {
    Pubkey::new_from_array([n; 32])
}
impl Fixture {
    fn new(intermediate: bool) -> Self {
        let owner = key(90);
        let program = key(91);
        let nonce = Pubkey::find_program_address(&[b"stocklana", owner.as_ref()], &program).0;
        let mut n = vec![0; 64];
        n[..8].copy_from_slice(b"SKEWSEQ1");
        n[8..40].copy_from_slice(owner.as_ref());
        n[40..48].copy_from_slice(&7u64.to_le_bytes());
        let mut bank = BTreeMap::from([
            (
                owner,
                Row {
                    owner: Pubkey::default(),
                    executable: false,
                    data: vec![],
                },
            ),
            (
                nonce,
                Row {
                    owner: program,
                    executable: false,
                    data: n,
                },
            ),
            (
                TOKEN,
                Row {
                    owner: Pubkey::default(),
                    executable: true,
                    data: vec![],
                },
            ),
        ]);
        let mut assets = Vec::new();
        for i in 0..3u8 {
            let token = key(100 + i);
            let mint = key(110 + i);
            let mut td = vec![0; 165];
            td[..32].copy_from_slice(mint.as_ref());
            td[32..64].copy_from_slice(owner.as_ref());
            td[64..72].copy_from_slice(&dex::MAX_INPUT.to_le_bytes());
            td[108] = 1;
            let mut md = vec![0; 82];
            md[45] = 1;
            bank.insert(
                token,
                Row {
                    owner: TOKEN,
                    executable: false,
                    data: td,
                },
            );
            bank.insert(
                mint,
                Row {
                    owner: TOKEN,
                    executable: false,
                    data: md,
                },
            );
            assets.push(TokenAsset {
                token,
                mint,
                token_program: TOKEN,
            });
        }
        let venue = Venue::RaydiumAmmV4;
        let id = Pubkey::from_str(venue.program()).unwrap();
        bank.insert(
            id,
            Row {
                owner: Pubkey::default(),
                executable: true,
                data: vec![],
            },
        );
        let pairs = if intermediate {
            vec![(0, 1), (1, 2)]
        } else {
            vec![(0, 2), (0, 2)]
        };
        let legs = pairs
            .iter()
            .enumerate()
            .map(|(i, &(src, dst))| {
                let mut accounts = (0..8)
                    .map(|j| key(10 + (i * 10 + j) as u8))
                    .collect::<Vec<_>>();
                for k in &accounts {
                    bank.insert(
                        *k,
                        Row {
                            owner: id,
                            executable: false,
                            data: vec![],
                        },
                    );
                }
                accounts[5] = assets[src].token;
                accounts[6] = assets[dst].token;
                accounts[7] = owner;
                SwapLeg {
                    venue,
                    direction: true,
                    source: assets[src].token,
                    destination: assets[dst].token,
                    budget: if i == 0 && !intermediate {
                        Budget::Exact(37_123_457)
                    } else {
                        Budget::Remaining
                    },
                    accounts,
                }
            })
            .collect();
        Self {
            program,
            bank,
            spec: SwapGraph {
                owner,
                sequence: 7,
                input_atoms: 100_000_003,
                minimum_output_atoms: 1,
                deadline_slot: 100,
                input: assets[0],
                output: assets[2],
                intermediates: if intermediate {
                    vec![assets[1]]
                } else {
                    vec![]
                },
                legs,
            },
        }
    }
    fn compile(&self) -> Result<Instruction> {
        compile_swap_graph(self.program, &self.spec, |k| {
            let row = self.bank.get(k).ok_or("missing test account")?;
            Ok(AccountView {
                owner: row.owner,
                executable: row.executable,
                data: &row.data,
            })
        })
    }
    fn native_candidates() -> Self {
        let mut f = Self::new(false);
        let venue = Venue::RaydiumClmm;
        let id = Pubkey::from_str(venue.program()).unwrap();
        f.bank.insert(
            id,
            Row {
                owner: Pubkey::default(),
                executable: true,
                data: vec![],
            },
        );
        for (i, leg) in f.spec.legs.iter_mut().enumerate() {
            let mut accounts = (0..14)
                .map(|j| key(130 + (i * 20 + j) as u8))
                .collect::<Vec<_>>();
            for address in &accounts {
                f.bank.insert(
                    *address,
                    Row {
                        owner: id,
                        executable: false,
                        data: vec![],
                    },
                );
            }
            accounts[0] = f.spec.owner;
            accounts[3] = f.spec.input.token;
            accounts[4] = f.spec.output.token;
            leg.venue = venue;
            leg.accounts = accounts;
            leg.budget = Budget::Remaining;
        }
        f
    }
    fn economic_candidates() -> Self {
        let mut f = Self::new(true);
        let venue = Venue::RaydiumClmm;
        let id = Pubkey::from_str(venue.program()).unwrap();
        f.bank.insert(
            id,
            Row {
                owner: Pubkey::default(),
                executable: true,
                data: vec![],
            },
        );
        for (i, leg) in f.spec.legs.iter_mut().enumerate() {
            let mut accounts = (0..14)
                .map(|j| key(180 + (i * 20 + j) as u8))
                .collect::<Vec<_>>();
            for address in &accounts {
                f.bank.insert(
                    *address,
                    Row {
                        owner: id,
                        executable: false,
                        data: vec![],
                    },
                );
            }
            leg.venue = venue;
            leg.source = f.spec.input.token;
            leg.destination = if i == 0 {
                f.spec.intermediates[0].token
            } else {
                f.spec.output.token
            };
            accounts[0] = f.spec.owner;
            accounts[3] = leg.source;
            accounts[4] = leg.destination;
            leg.accounts = accounts;
            leg.budget = Budget::Remaining;
        }
        f
    }
    fn reflow(&self, calls: u8, seed: Option<u64>) -> Result<Instruction> {
        compile_reflow_graph(self.program, &self.spec, calls, seed, |key| {
            let row = self.bank.get(key).ok_or("missing test account")?;
            Ok(AccountView {
                owner: row.owner,
                executable: row.executable,
                data: &row.data,
            })
        })
    }
    fn economic(&self, calls: u8) -> Result<Instruction> {
        compile_economic_reflow_graph(self.program, &self.spec, calls, |key| {
            let row = self.bank.get(key).ok_or("missing test account")?;
            Ok(AccountView {
                owner: row.owner,
                executable: row.executable,
                data: &row.data,
            })
        })
    }
}

#[test]
fn economic_reflow_preserves_distinct_product_mints_in_one_candidate_graph() {
    let f = Fixture::economic_candidates();
    let ix = f.economic(16).unwrap();
    let graph = Graph::decode(&ix.data, ix.accounts.len()).unwrap();
    assert_eq!(ix.data[0], 4);
    assert_eq!(graph.asset_count, 3);
    assert_eq!(graph.reflow_calls, 16);
    assert!(graph.legs[..graph.leg_count]
        .iter()
        .flatten()
        .all(|leg| leg.source == 0));
    assert_eq!(graph.legs[0].unwrap().destination, 1);
    assert_eq!(graph.legs[1].unwrap().destination, 2);

    let mut composed = f;
    composed.spec.legs[1].source = composed.spec.intermediates[0].token;
    composed.spec.legs[1].accounts[3] = composed.spec.intermediates[0].token;
    let ix = composed.economic(16).unwrap();
    let graph = Graph::decode(&ix.data, ix.accounts.len()).unwrap();
    assert_eq!(graph.legs[1].unwrap().source, 1);
    assert_eq!(graph.legs[1].unwrap().destination, 2);

    let mut unrooted = composed;
    unrooted.spec.legs[0].destination = unrooted.spec.output.token;
    unrooted.spec.legs[0].accounts[4] = unrooted.spec.output.token;
    assert!(unrooted.economic(16).unwrap_err().contains("bounded path"));
}

#[test]
fn reflow_candidates_are_not_a_fixed_allocation_and_preserve_signed_seed() {
    let f = Fixture::native_candidates();
    assert!(f.compile().is_err()); // Multiple candidate remainders are not a split.
    for seed in [None, Some(90_000_000)] {
        let ix = f.reflow(8, seed).unwrap();
        let graph = Graph::decode(&ix.data, ix.accounts.len()).unwrap();
        assert_eq!(ix.data[0], if seed.is_some() { 9 } else { 4 });
        assert_eq!(graph.input, f.spec.input_atoms);
        assert_eq!(graph.reflow_calls, 8);
        assert_eq!(graph.seed_input, seed.unwrap_or(0));
        assert!(graph.legs[..graph.leg_count]
            .iter()
            .flatten()
            .all(|leg| leg.budget == u64::MAX));
    }
    let mut f = f;
    // Shared read-only configuration is allowed; independent writable market
    // accounts are required by the marginal optimizer's model.
    f.spec.legs[1].accounts[1] = f.spec.legs[0].accounts[1];
    assert!(f.reflow(8, None).is_ok());
    f.spec.legs[1].accounts[2] = f.spec.legs[0].accounts[2];
    assert!(f.reflow(8, None).unwrap_err().contains("shared market"));
}

#[test]
fn reflow_seed_work_caps_and_unadmitted_curves_reject() {
    let f = Fixture::native_candidates();
    for calls in [0, 7, 65, 255] {
        assert!(f.reflow(calls, None).is_err());
    }
    for seed in [0, f.spec.input_atoms, u64::MAX] {
        assert!(f.reflow(8, Some(seed)).is_err());
    }
    let mut f = f;
    f.spec.legs[0].budget = Budget::Exact(50_000_000);
    assert!(f.reflow(8, Some(50_000_001)).is_err());
    assert!(f.reflow(8, Some(50_000_000)).is_ok());
    f.spec.legs[0].budget = Budget::Exact(0);
    assert!(f.reflow(8, None).is_err());
    let f = Fixture::new(false);
    assert!(f.reflow(8, None).unwrap_err().contains("native candidate"));
    let f = Fixture::new(true);
    assert!(f.reflow(8, None).unwrap_err().contains("direct candidate"));
}

#[test]
fn integer_allocations_and_observed_intermediate_drain_survive_wire() {
    let f = Fixture::new(false);
    let ix = f.compile().unwrap();
    let graph = Graph::decode(&ix.data, ix.accounts.len()).unwrap();
    assert_eq!(graph.input, 100_000_003);
    assert_eq!(graph.sequence, 7);
    assert_eq!(graph.legs[0].unwrap().budget, 37_123_457);
    assert_eq!(graph.legs[1].unwrap().budget, u64::MAX);
    assert_eq!(ix.accounts.iter().filter(|a| a.is_signer).count(), 1);
    assert!(
        !ix.accounts
            .iter()
            .find(|a| a.pubkey == f.spec.input.mint)
            .unwrap()
            .is_writable
    );
    let intermediate = Fixture::new(true);
    let ix = intermediate.compile().unwrap();
    let graph = Graph::decode(&ix.data, ix.accounts.len()).unwrap();
    assert_eq!(graph.asset_count, 3);
    assert_eq!(graph.legs[1].unwrap().source, 1);
    assert_eq!(graph.legs[1].unwrap().budget, u64::MAX);
    // A partial raw amount cannot stand in for the actual bridge output.
    let mut bad = intermediate;
    bad.spec.legs[1].budget = Budget::Exact(1);
    assert!(bad.compile().unwrap_err().contains("must drain"));
}

#[test]
fn malformed_allocations_and_execution_order_are_rejected_before_simulation() {
    for amount in [0, 100_000_003, dex::MAX_INPUT + 1] {
        let mut f = Fixture::new(false);
        f.spec.legs[0].budget = Budget::Exact(amount);
        assert!(f.compile().is_err());
    }
    let mut f = Fixture::new(false);
    f.spec.legs[0].budget = Budget::Remaining;
    assert!(f.compile().unwrap_err().contains("last consumer"));
    let mut f = Fixture::new(true);
    f.spec.legs.swap(0, 1);
    assert!(f.compile().unwrap_err().contains("producer order"));
    let mut f = Fixture::new(false);
    f.spec.legs[1].source = f.spec.output.token;
    f.spec.legs[1].destination = f.spec.input.token;
    assert!(f.compile().unwrap_err().contains("topological"));
    let mut f = Fixture::new(false);
    f.spec.legs[1].budget = Budget::Exact(62_876_546);
    assert!(f.compile().is_ok());
    f.spec.legs[1].budget = Budget::Exact(62_876_545);
    assert!(f.compile().unwrap_err().contains("conservation"));
}

#[test]
fn bank_nonce_authority_and_privilege_aliases_fail_closed() {
    for offset in [72, 121, 129] {
        let mut f = Fixture::new(false);
        f.bank.get_mut(&f.spec.input.token).unwrap().data[offset] = 1;
        assert!(f.compile().unwrap_err().contains("asset bank binding"));
    }
    let mut f = Fixture::new(false);
    f.spec.sequence += 1;
    assert!(f.compile().unwrap_err().contains("nonce binding"));
    let mut f = Fixture::new(false);
    f.bank.get_mut(&f.spec.input.token).unwrap().data[32] ^= 1;
    assert!(f.compile().unwrap_err().contains("asset bank binding"));
    let mut f = Fixture::new(false);
    f.spec.legs[0].accounts[7] = key(33);
    assert!(f.compile().unwrap_err().contains("venue account binding"));
    let mut f = Fixture::new(false);
    f.spec.legs[0].accounts[3] = f.spec.output.token;
    assert!(f.compile().unwrap_err().contains("asset alias"));
    let mut f = Fixture::new(false);
    f.spec.legs[0].accounts[3] = f.spec.input.mint;
    assert!(f.compile().unwrap_err().contains("protected account"));
    let mut f = Fixture::new(false);
    let unknown_wallet_token = key(101);
    f.spec.legs[0].accounts[3] = unknown_wallet_token;
    assert!(f.compile().unwrap_err().contains("undeclared wallet token"));
    let mut f = Fixture::new(false);
    f.bank.remove(&f.spec.legs[0].accounts[2]);
    assert!(f.compile().is_err());
}
