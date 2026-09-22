//! Bounded, typed swap graph ABI. No caller-provided CPI instruction bytes.
use crate::{u64_at, Error, Result};

/// One funding/cash asset plus up to four issuer products.  Fixed swap graphs
/// normally use two to four assets; opcode-18 v2 uses the fifth slot to compare
/// four economically equivalent products without pretending their mints match.
pub const MAX_ASSETS: usize = 5;
pub const MAX_LEGS: usize = 4;
pub const MAX_CANDIDATES: usize = 8;
pub const MAX_CPI_ACCOUNTS: usize = 24;
pub const MAX_ACCOUNTS: usize = 64;
pub const HEADER_LEN: usize = 36;
pub const LEG_HEADER_LEN: usize = 14;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Venue {
    Phoenix = 0,
    RaydiumCpmm = 1,
    OrcaWhirlpool = 2,
    RaydiumClmm = 3,
    MeteoraDlmm = 4,
    MeteoraDammV2 = 5,
    RaydiumAmmV4 = 6,
    ByrealClmm = 7,
    Riptide = 8,
}
impl TryFrom<u8> for Venue {
    type Error = Error;
    fn try_from(v: u8) -> Result<Self> {
        match v {
            0 => Ok(Self::Phoenix),
            1 => Ok(Self::RaydiumCpmm),
            2 => Ok(Self::OrcaWhirlpool),
            3 => Ok(Self::RaydiumClmm),
            4 => Ok(Self::MeteoraDlmm),
            5 => Ok(Self::MeteoraDammV2),
            6 => Ok(Self::RaydiumAmmV4),
            7 => Ok(Self::ByrealClmm),
            8 => Ok(Self::Riptide),
            _ => Err(Error::Unsupported),
        }
    }
}
impl Venue {
    pub const COUNT: u8 = 9;
    pub const fn program(self) -> &'static str {
        match self {
            Self::Phoenix => crate::PHOENIX_ID,
            Self::RaydiumCpmm => crate::RAYDIUM_ID,
            Self::OrcaWhirlpool => "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",
            Self::RaydiumClmm => "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK",
            Self::MeteoraDlmm => "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo",
            Self::MeteoraDammV2 => "cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG",
            Self::RaydiumAmmV4 => "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8",
            Self::ByrealClmm => "REALQqNEomY6cQGZJUGwywTBD2UmDT32rZcNnfxQ5N2",
            Self::Riptide => "riptK81hDxhe5pW5jSzSM9iRA8azgEgLJ4dXkPtBS7j",
        }
    }
    pub const fn account_bounds(self) -> (usize, usize) {
        match self {
            Self::Phoenix => (9, 9),
            Self::RaydiumCpmm => (13, 13),
            Self::OrcaWhirlpool => (15, 15),
            Self::RaydiumClmm | Self::ByrealClmm => (14, 20),
            Self::MeteoraDlmm => (17, 22),
            Self::MeteoraDammV2 => (14, 14),
            Self::RaydiumAmmV4 => (8, 8),
            Self::Riptide => (12, 13),
        }
    }
    /// CPI positions: pool, signer, source, destination. Direction is A -> B.
    pub const fn bindings(self, direction: bool) -> (usize, usize, usize, usize) {
        match self {
            Self::Phoenix => (
                2,
                3,
                if direction { 4 } else { 5 },
                if direction { 5 } else { 4 },
            ),
            Self::RaydiumCpmm => (3, 0, 4, 5),
            Self::OrcaWhirlpool => (
                4,
                3,
                if direction { 7 } else { 9 },
                if direction { 9 } else { 7 },
            ),
            Self::RaydiumClmm | Self::ByrealClmm => (2, 0, 3, 4),
            Self::MeteoraDlmm => (0, 10, 4, 5),
            Self::MeteoraDammV2 => (1, 8, 2, 3),
            Self::RaydiumAmmV4 => (1, 7, 5, 6),
            Self::Riptide => (
                1,
                0,
                if direction { 4 } else { 5 },
                if direction { 5 } else { 4 },
            ),
        }
    }
    pub fn writable(self, i: usize) -> bool {
        match self {
            Self::Phoenix => [2, 4, 5, 6, 7].contains(&i),
            Self::RaydiumCpmm => [0, 3, 4, 5, 6, 7, 12].contains(&i),
            Self::OrcaWhirlpool => [4, 7, 8, 9, 10, 11, 12, 13, 14].contains(&i),
            Self::RaydiumClmm | Self::ByrealClmm => [2, 3, 4, 5, 6, 7].contains(&i) || i >= 13,
            Self::MeteoraDlmm => [0, 1, 2, 3, 4, 5, 8, 9].contains(&i) || i >= 16,
            Self::MeteoraDammV2 => [1, 2, 3, 4, 5, 11].contains(&i),
            Self::RaydiumAmmV4 => [1, 3, 4, 5, 6].contains(&i),
            Self::Riptide => [1, 4, 5, 6, 7].contains(&i),
        }
    }
    /// Byreal v3 appends read-only Pyth oracle accounts after its owned tick
    /// accounts. Avoid acquiring write locks on shared oracle feeds.
    pub fn writable_with_owner(self, position: usize, owned_by_venue: bool) -> bool {
        self.writable(position) && !(self == Self::ByrealClmm && position >= 13 && !owned_by_venue)
    }
    /// Plain exact-input swap only. Phoenix needs its pool's lot sizes separately.
    /// Zero price limit lets each CLMM use its canonical boundary. Account/tick
    /// bounds and the transaction CU limit bound traversals, not a fake tick cap.
    pub fn swap_data(self, input: u64, direction: bool, dst: &mut [u8; 80]) -> Result<usize> {
        if input == 0 || input > crate::MAX_INPUT {
            return Err(Error::Amount);
        }
        dst.fill(0);
        let (disc, len): (&[u8], usize) = match self {
            Self::Phoenix => return Err(Error::Unsupported),
            Self::RaydiumCpmm => (&[143, 190, 90, 218, 196, 30, 51, 222], 24),
            Self::OrcaWhirlpool => (&[43, 4, 237, 11, 26, 201, 30, 98], 43),
            Self::RaydiumClmm => (&[43, 4, 237, 11, 26, 201, 30, 98], 41),
            Self::ByrealClmm => (&[229, 46, 213, 132, 105, 40, 40, 228], 41),
            Self::MeteoraDlmm => (&[65, 75, 63, 76, 235, 91, 91, 136], 28),
            Self::MeteoraDammV2 => (&[65, 75, 63, 76, 235, 91, 91, 136], 25),
            Self::RaydiumAmmV4 => (&[16], 17),
            Self::Riptide => (&[2], 12),
        };
        dst[..disc.len()].copy_from_slice(disc);
        dst[disc.len()..disc.len() + 8].copy_from_slice(&input.to_le_bytes());
        match self {
            Self::OrcaWhirlpool => {
                dst[40] = 1;
                dst[41] = u8::from(direction);
            }
            Self::RaydiumClmm | Self::ByrealClmm => {
                dst[40] = 1;
            }
            Self::Riptide => {
                dst[9] = u8::from(direction);
            } // None slippage; graph verifies final min_out.
            _ => {}
        }
        Ok(len)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Asset {
    pub token: u8,
    pub mint: u8,
    pub program: u8,
}
#[derive(Clone, Copy, Debug)]
pub struct Leg<'a> {
    pub venue: Venue,
    pub source: usize,
    pub destination: usize,
    pub direction: bool,
    pub budget: u64,
    pub program: u8,
    pub accounts: &'a [u8],
}
pub struct Graph<'a> {
    pub sequence: u64,
    pub input: u64,
    pub min_out: u64,
    pub deadline: u64,
    pub assets: [Asset; MAX_ASSETS],
    pub asset_count: usize,
    pub legs: [Option<Leg<'a>>; MAX_CANDIDATES],
    pub leg_count: usize,
    /// Opcodes 4/9: bounded residual allocation; zero means fixed graph.
    pub reflow_calls: u8,
    /// Opcode 9 only: signed first-candidate input; later rounds solve observed residual.
    pub seed_input: u64,
}
impl<'a> Graph<'a> {
    pub fn decode(d: &'a [u8], account_count: usize) -> Result<Self> {
        if d.len() < HEADER_LEN
            || ![2, 4, 9].contains(&d[0])
            || (d[0] == 2 && d[3] != 0)
            || ([4, 9].contains(&d[0]) && !(8..=64).contains(&d[3]))
            || (d[0] == 9 && d[1] != 2)
            || !(2..=MAX_ASSETS).contains(&(d[1] as usize))
            || !(1..=if d[0] != 2 { MAX_CANDIDATES } else { MAX_LEGS }).contains(&(d[2] as usize))
            || !(6..=MAX_ACCOUNTS).contains(&account_count)
        {
            return Err(Error::Layout);
        }
        let mut g = Self {
            sequence: u64_at(d, 4)?,
            input: u64_at(d, 12)?,
            min_out: u64_at(d, 20)?,
            deadline: u64_at(d, 28)?,
            assets: [Asset::default(); MAX_ASSETS],
            asset_count: d[1] as usize,
            legs: [None; MAX_CANDIDATES],
            leg_count: d[2] as usize,
            reflow_calls: if d[0] != 2 { d[3] } else { 0 },
            seed_input: 0,
        };
        if g.input == 0 || g.input > crate::MAX_INPUT || g.min_out == 0 {
            return Err(Error::Amount);
        }
        let mut cursor = HEADER_LEN;
        for asset in &mut g.assets[..g.asset_count] {
            let s = d.get(cursor..cursor + 3).ok_or(Error::Layout)?;
            if s.iter().any(|i| *i < 2 || *i as usize >= account_count) {
                return Err(Error::Layout);
            }
            *asset = Asset {
                token: s[0],
                mint: s[1],
                program: s[2],
            };
            cursor += 3;
        }
        if d[0] == 9 {
            g.seed_input = u64_at(d, cursor)?;
            cursor += 8;
            if g.seed_input == 0 || g.seed_input >= g.input {
                return Err(Error::Amount);
            }
        }
        for leg in &mut g.legs[..g.leg_count] {
            let s = d
                .get(cursor..cursor + LEG_HEADER_LEN)
                .ok_or(Error::Layout)?;
            let venue = Venue::try_from(s[0])?;
            let n = s[13] as usize;
            let (lo, hi) = venue.account_bounds();
            // Topological asset order: original input=0, final output=last.
            if s[1] >= s[2]
                || s[2] as usize >= g.asset_count
                || s[3] > 1
                || s[12] as usize >= account_count
                || n < lo
                || n > hi
            {
                return Err(Error::Layout);
            }
            let budget = u64_at(s, 4)?;
            if budget == 0 || (budget > crate::MAX_INPUT && budget != u64::MAX) {
                return Err(Error::Amount);
            }
            cursor += LEG_HEADER_LEN;
            let indices = d.get(cursor..cursor + n).ok_or(Error::Layout)?;
            if indices
                .iter()
                .any(|i| *i as usize >= account_count || *i == 1)
            {
                return Err(Error::Layout);
            }
            *leg = Some(Leg {
                venue,
                source: s[1] as usize,
                destination: s[2] as usize,
                direction: s[3] == 1,
                budget,
                program: s[12],
                accounts: indices,
            });
            cursor += n;
        }
        if g.seed_input > g.legs[0].as_ref().ok_or(Error::Layout)?.budget {
            return Err(Error::Amount);
        }
        if cursor != d.len() {
            return Err(Error::Layout);
        }
        Ok(g)
    }
}
