use crate::matrix::*;
use skew_native::{clmm::ClmmCurve, Error, Result, TransferFee};
use solana_instruction::Instruction;
use stocklana_adapters::{self as dex, graph::Venue};

pub enum Curve {
    Orca(skew_native::orca::OrcaCurve),
    Damm(skew_native::damm::DammCurve, bool),
    Dlmm(skew_native::dlmm::DlmmCurve, bool),
    Clmm(ClmmCurve, bool),
    Cpmm(
        Box<dex::RaydiumPool>,
        Vec<u8>,
        [u64; 2],
        bool,
        TransferFee,
        TransferFee,
    ),
    Phoenix(Vec<u8>, bool, u64, u64),
    AmmV4 {
        input: u64,
        output: u64,
        numerator: u64,
        denominator: u64,
    },
}
fn map<T>(v: dex::Result<T>) -> Result<T> {
    v.map_err(|_| Error::Unsupported)
}
impl Curve {
    pub fn decode(
        a: &Accounts,
        v: Venue,
        ix: &Instruction,
        dir: bool,
        slot: u64,
        time: u64,
        epoch: u64,
    ) -> Result<Self> {
        if ix.program_id != pk(v.program()) {
            return Err(Error::Layout);
        }
        let (pi, _, src, dst) = v.bindings(dir);
        let acc = |i: usize| -> Result<&solana_account::Account> {
            let k = ix.accounts.get(i).ok_or(Error::Layout)?.pubkey;
            a.iter()
                .find(|x| x.0 == k)
                .map(|x| &x.1)
                .ok_or(Error::Layout)
        };
        let pool = acc(pi)?;
        if pool.owner != ix.program_id {
            return Err(Error::Layout);
        }
        let mint_fee = |i: usize| -> Result<TransferFee> {
            let mint = map(dex::key(&acc(i)?.data, 0))?;
            let m = a
                .iter()
                .find(|x| x.0.to_bytes() == mint)
                .ok_or(Error::Layout)?;
            TransferFee::decode(&m.1.data, epoch)
        };
        let input_fee = mint_fee(src)?;
        let output_fee = mint_fee(dst)?;
        match v {
            Venue::OrcaWhirlpool => {
                if ix.accounts.len() != 15
                    || map(dex::key(&acc(src)?.data, 0))?
                        != map(dex::key(&pool.data, if dir { 101 } else { 181 }))?
                    || map(dex::key(&acc(dst)?.data, 0))?
                        != map(dex::key(&pool.data, if dir { 181 } else { 101 }))?
                {
                    return Err(Error::Layout);
                }
                let mut arrays = Vec::new();
                let mut seen = std::collections::BTreeSet::new();
                for i in 11..14 {
                    if seen.insert(ix.accounts[i].pubkey) {
                        if acc(i)?.owner != ix.program_id {
                            return Err(Error::Unsupported);
                        }
                        arrays.push(acc(i)?.data.as_slice());
                    }
                }
                let oracle = acc(14)?;
                Ok(Self::Orca(skew_native::orca::OrcaCurve::decode(
                    ix.accounts[pi].pubkey.to_bytes(),
                    &pool.data,
                    &arrays,
                    if oracle.owner == ix.program_id {
                        Some(&oracle.data)
                    } else {
                        None
                    },
                    [input_fee, output_fee],
                    time,
                    dir,
                )?))
            }

            Venue::MeteoraDammV2 => {
                let input = map(dex::key(&acc(src)?.data, 0))?;
                let ma = map(dex::key(&pool.data, 168))?;
                let mb = map(dex::key(&pool.data, 200))?;
                let zero = input == ma;
                if (!zero && input != mb)
                    || map(dex::key(&acc(dst)?.data, 0))? != if zero { mb } else { ma }
                {
                    return Err(Error::Layout);
                }
                Ok(Self::Damm(
                    skew_native::damm::DammCurve::decode(
                        &pool.data,
                        [input_fee, output_fee],
                        slot,
                        time,
                    )?,
                    zero,
                ))
            }
            Venue::MeteoraDlmm => {
                let input_mint = map(dex::key(&acc(src)?.data, 0))?;
                let x_to_y = input_mint == map(dex::key(&pool.data, 88))?;
                if !x_to_y && input_mint != map(dex::key(&pool.data, 120))? {
                    return Err(Error::Layout);
                }
                let arrays: Vec<_> = ix
                    .accounts
                    .iter()
                    .skip(16)
                    .filter_map(|k| a.iter().find(|x| x.0 == k.pubkey))
                    .filter(|(_, x)| x.owner == ix.program_id && x.data.len() == 10136)
                    .map(|(_, x)| x.data.as_slice())
                    .collect();
                Ok(Self::Dlmm(
                    skew_native::dlmm::DlmmCurve::decode(
                        ix.accounts[pi].pubkey.to_bytes(),
                        &pool.data,
                        &arrays,
                        [input_fee, output_fee],
                        slot,
                        time,
                    )?,
                    x_to_y,
                ))
            }
            Venue::RaydiumClmm | Venue::ByrealClmm => {
                if map(dex::key(&pool.data, 9))? != ix.accounts[1].pubkey.to_bytes()
                    || acc(1)?.owner != ix.program_id
                {
                    return Err(Error::Layout);
                }
                let arrays: Vec<_> = ix
                    .accounts
                    .iter()
                    .skip(13)
                    .filter_map(|m| a.iter().find(|x| x.0 == m.pubkey))
                    .filter(|(_, x)| x.owner == ix.program_id && x.data.len() != 1832)
                    .map(|(_, x)| x.data.as_slice())
                    .collect();
                let input_mint = map(dex::key(&acc(src)?.data, 0))?;
                let zero = input_mint == map(dex::key(&pool.data, 73))?;
                if !zero && input_mint != map(dex::key(&pool.data, 105))? {
                    return Err(Error::Layout);
                }
                let curve = if v == Venue::ByrealClmm {
                    ClmmCurve::decode_byreal(
                        ix.accounts[pi].pubkey.to_bytes(),
                        &pool.data,
                        &acc(1)?.data,
                        &arrays,
                        [input_fee, output_fee],
                        time,
                    )?
                } else {
                    ClmmCurve::decode_at(
                        ix.accounts[pi].pubkey.to_bytes(),
                        &pool.data,
                        &acc(1)?.data,
                        &arrays,
                        [input_fee, output_fee],
                        time,
                    )?
                };
                Ok(Self::Clmm(curve, zero))
            }
            Venue::RaydiumCpmm => {
                let p = map(dex::RaydiumPool::decode(&pool.data, time))?;
                if p.config != ix.accounts[2].pubkey.to_bytes() {
                    return Err(Error::Layout);
                }
                let mut balances = [0; 2];
                for (i, balance) in balances.iter_mut().enumerate() {
                    let k = solana_pubkey::Pubkey::new_from_array(p.vaults[i]);
                    *balance = map(dex::u64_at(&get(a, &k).data, 64))?;
                }
                let zero = map(dex::key(&acc(src)?.data, 0))? == p.mints[0];
                Ok(Self::Cpmm(
                    Box::new(p),
                    acc(2)?.data.clone(),
                    balances,
                    zero,
                    input_fee,
                    output_fee,
                ))
            }
            Venue::Phoenix => {
                map(dex::PhoenixBook::decode(&pool.data))?;
                Ok(Self::Phoenix(pool.data.clone(), dir, slot, time))
            }
            Venue::RaydiumAmmV4 => {
                if pool.data.len() != 752 || ![1, 6].contains(&map(dex::u64_at(&pool.data, 0))?) {
                    return Err(Error::Unsupported);
                }
                let base = map(dex::key(&pool.data, 336))?;
                if base != ix.accounts[3].pubkey.to_bytes() {
                    return Err(Error::Layout);
                }
                let b = map(dex::u64_at(&acc(3)?.data, 64))?
                    .checked_sub(map(dex::u64_at(&pool.data, 192))?)
                    .ok_or(Error::Arithmetic)?;
                let q = map(dex::u64_at(&acc(4)?.data, 64))?
                    .checked_sub(map(dex::u64_at(&pool.data, 200))?)
                    .ok_or(Error::Arithmetic)?;
                let base_input =
                    map(dex::key(&acc(src)?.data, 0))? == map(dex::key(&acc(3)?.data, 0))?;
                let numerator = map(dex::u64_at(&pool.data, 176))?;
                let denominator = map(dex::u64_at(&pool.data, 184))?;
                if numerator >= denominator
                    || denominator == 0
                    || input_fee != TransferFee::default()
                    || output_fee != TransferFee::default()
                {
                    return Err(Error::Unsupported);
                }
                Ok(Self::AmmV4 {
                    input: if base_input { b } else { q },
                    output: if base_input { q } else { b },
                    numerator,
                    denominator,
                })
            }
            _ => Err(Error::Unsupported),
        }
    }
    pub fn quote(&self, input: u64) -> Result<u64> {
        if input == 0 || input > dex::MAX_INPUT {
            return Err(Error::Capacity);
        }
        match self {
            Self::Orca(c) => c.quote(input),
            Self::Damm(c, d) => c.quote(input, *d),
            Self::Dlmm(c, d) => c.quote(input, *d),
            Self::Clmm(c, d) => c.quote(input, *d),
            Self::Cpmm(p, c, b, d, inf, outf) => {
                outf.net(map(p.quote(c, *b, *d, inf.net(input)?))?)
            }
            Self::Phoenix(b, d, s, t) => {
                let f = map(map(dex::PhoenixBook::decode(b))?.quote(
                    &WALLET.to_bytes(),
                    *d,
                    input,
                    16,
                    *s,
                    *t,
                ))?;
                if f.input == input {
                    Ok(f.output)
                } else {
                    Err(Error::Capacity)
                }
            }
            Self::AmmV4 {
                input: r,
                output: o,
                numerator: n,
                denominator: d,
            } => {
                let net = input
                    .checked_sub(
                        (u128::from(input) * u128::from(*n)).div_ceil(u128::from(*d)) as u64,
                    )
                    .ok_or(Error::Arithmetic)?;
                let out =
                    (u128::from(net) * u128::from(*o) / (u128::from(*r) + u128::from(net))) as u64;
                if out == 0 {
                    Err(Error::Capacity)
                } else {
                    Ok(out)
                }
            }
        }
    }
}
