//! Byte-level adapters for the pinned Phoenix Legacy and Raydium CPMM ABIs.
//! No allocation, unchecked casts, floating point, or Token-2022 assumptions.
#![no_std]

pub mod graph;

pub const PHOENIX_ID: &str = "PhoeNiXZ8ByJGLkxNfZRnkUfjvmuYqLR89jjFHGqdXY";
pub const RAYDIUM_ID: &str = "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C";
pub const TOKEN_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const MAX_MATCHES: u64 = 16;
pub const MAX_INPUT: u64 = 100_000_000_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Layout,
    Status,
    Arithmetic,
    Amount,
    Unsupported,
}
pub type Result<T> = core::result::Result<T, Error>;

pub fn key(data: &[u8], offset: usize) -> Result<[u8; 32]> {
    data.get(offset..offset + 32)
        .ok_or(Error::Layout)?
        .try_into()
        .map_err(|_| Error::Layout)
}
pub fn u64_at(data: &[u8], offset: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(
        data.get(offset..offset + 8)
            .ok_or(Error::Layout)?
            .try_into()
            .map_err(|_| Error::Layout)?,
    ))
}
pub fn token_amount(data: &[u8]) -> Result<u64> {
    if data.len() != 165 || data[108] != 1 {
        return Err(Error::Layout);
    }
    u64_at(data, 64)
}

#[derive(Clone, Copy, Debug)]
pub struct PhoenixHeader {
    pub base_mint: [u8; 32],
    pub quote_mint: [u8; 32],
    pub base_vault: [u8; 32],
    pub quote_vault: [u8; 32],
    pub base_lot: u64,
    pub quote_lot: u64,
    pub sequence: u64,
}
impl PhoenixHeader {
    pub fn decode(data: &[u8]) -> Result<Self> {
        if data.len() < 576 || data[..8] != [119, 223, 113, 115, 183, 32, 88, 113] {
            return Err(Error::Layout);
        }
        if u64_at(data, 8)? != 1 {
            return Err(Error::Status);
        }
        let result = Self {
            base_mint: key(data, 48)?,
            quote_mint: key(data, 128)?,
            base_vault: key(data, 80)?,
            quote_vault: key(data, 160)?,
            base_lot: u64_at(data, 112)?,
            quote_lot: u64_at(data, 192)?,
            sequence: u64_at(data, 272)?,
        };
        if result.base_lot == 0 || result.quote_lot == 0 {
            return Err(Error::Layout);
        }
        Ok(result)
    }
}

/// Phoenix Swap + Borsh ImmediateOrCancel. Wallet funds only, Abort self-trade,
/// explicit match bound, slot expiry, no minimum local fill (residual may reflow).
pub fn phoenix_ioc(
    header: &PhoenixHeader,
    sell_base: bool,
    amount: u64,
    matches: u64,
    last_slot: u64,
) -> Result<[u8; 80]> {
    if amount == 0 || amount > MAX_INPUT || matches == 0 || matches > MAX_MATCHES {
        return Err(Error::Amount);
    }
    let mut data = [0u8; 80];
    // Swap=0, IOC=2, Bid=0/Ask=1, None price limit.
    data[1] = 2;
    data[2] = u8::from(sell_base);
    let lots = amount
        / if sell_base {
            header.base_lot
        } else {
            header.quote_lot
        };
    if lots == 0 {
        return Err(Error::Amount);
    }
    // fields: base lots, quote lots, min base lots, min quote lots, Abort=0.
    let offset = if sell_base { 4 } else { 12 };
    data[offset..offset + 8].copy_from_slice(&lots.to_le_bytes());
    data[37] = 1; // Some(match_limit)
    data[38..46].copy_from_slice(&matches.to_le_bytes());
    // client ID:16 bytes, wallet funds:0, Some(last_valid_slot).
    data[63] = 1;
    data[64..72].copy_from_slice(&last_slot.to_le_bytes());
    // final None timestamp at72. Serialized length is 73; trailing storage unused.
    Ok(data)
}
pub const PHOENIX_IOC_LEN: usize = 73;

/// Read-only walk of Sokoban 0.3 FIFO trees. At most 32 tree levels and 16
/// encountered orders; expired orders consume the same match budget as Phoenix.
/// The admission boundary excludes existing free input balances, since Phoenix
/// otherwise spends deposited funds before wallet funds, changing intent debit.
pub struct PhoenixBook<'a> {
    data: &'a [u8],
    pub header: PhoenixHeader,
    bids: usize,
    asks: usize,
    seats: usize,
    traders: usize,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Fill {
    pub input: u64,
    pub output: u64,
    pub encountered: u64,
}
fn u32_at(data: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        data.get(offset..offset + 4)
            .ok_or(Error::Layout)?
            .try_into()
            .map_err(|_| Error::Layout)?,
    ))
}
impl<'a> PhoenixBook<'a> {
    pub fn decode(data: &'a [u8]) -> Result<Self> {
        let header = PhoenixHeader::decode(data)?;
        let bids = u64_at(data, 16)? as usize;
        let asks = u64_at(data, 24)? as usize;
        let seats = u64_at(data, 32)? as usize;
        if ![512, 1024, 2048, 4096].contains(&bids)
            || asks != bids
            || ![128, 2 * bids + 1, 2 * bids + 129].contains(&seats)
        {
            return Err(Error::Unsupported);
        }
        let traders = 880 + 32 + bids * 64 + 32 + asks * 64;
        if data.len() != traders + 32 + seats * 144
            || u64_at(data, 832)? == 0
            || u64_at(data, 840)? == 0
            || u64_at(data, 856)? >= 10_000
        {
            return Err(Error::Layout);
        }
        Ok(Self {
            data,
            header,
            bids,
            asks,
            seats,
            traders,
        })
    }
    fn node(&self, tree: usize, index: u32, capacity: usize, stride: usize) -> Result<usize> {
        if index == 0 || index as usize > capacity {
            return Err(Error::Layout);
        }
        Ok(tree + 32 + (index as usize - 1) * stride)
    }
    pub fn check_wallet(&self, wallet: &[u8; 32], sell: bool) -> Result<()> {
        let mut index = u32_at(self.data, self.traders)?;
        for _ in 0..32 {
            if index == 0 {
                return Ok(());
            }
            let node = self.node(self.traders, index, self.seats, 144)?;
            let maker = key(self.data, node + 16)?;
            match wallet.cmp(&maker) {
                core::cmp::Ordering::Equal => {
                    let free = u64_at(self.data, node + if sell { 72 } else { 56 })?;
                    return if free == 0 {
                        Ok(())
                    } else {
                        Err(Error::Unsupported)
                    };
                }
                core::cmp::Ordering::Less => index = u32_at(self.data, node)?,
                core::cmp::Ordering::Greater => index = u32_at(self.data, node + 4)?,
            }
        }
        Err(Error::Layout)
    }
    pub fn quote(
        &self,
        wallet: &[u8; 32],
        sell: bool,
        input: u64,
        matches: u64,
        slot: u64,
        time: u64,
    ) -> Result<Fill> {
        if input == 0 || input > MAX_INPUT || matches == 0 || matches > MAX_MATCHES {
            return Err(Error::Amount);
        }
        self.check_wallet(wallet, sell)?;
        let units = u64_at(self.data, 832)?;
        let tick = u64_at(self.data, 840)?;
        let fee = u64_at(self.data, 856)?;
        let mut base_budget = if sell {
            input / self.header.base_lot
        } else {
            u64::MAX
        };
        let mut quote_budget = if sell {
            u64::MAX
        } else {
            let adjusted = (input / self.header.quote_lot)
                .checked_mul(units)
                .ok_or(Error::Arithmetic)?;
            let max = u128::from(u64::MAX);
            let divisor = max + (max * u128::from(fee)).div_ceil(10_000);
            u64::try_from(u128::from(adjusted) * max / divisor).map_err(|_| Error::Arithmetic)?
        };
        let tree = if sell { 880 } else { 880 + 32 + self.bids * 64 };
        let capacity = if sell { self.bids } else { self.asks };
        let mut index = u32_at(self.data, tree)?;
        let mut stack = [0u32; 32];
        let mut height = 0;
        let mut matched_base = 0u64;
        let mut matched_quote = 0u64;
        let mut encountered = 0;
        while encountered < matches && base_budget > 0 && quote_budget > 0 {
            while index != 0 {
                if height == 32 {
                    return Err(Error::Layout);
                }
                stack[height] = index;
                height += 1;
                index = u32_at(self.data, self.node(tree, index, capacity, 64)?)?;
            }
            if height == 0 {
                break;
            }
            height -= 1;
            let node = self.node(tree, stack[height], capacity, 64)?;
            index = u32_at(self.data, node + 4)?;
            encountered += 1;
            let price = u64_at(self.data, node + 16)?;
            let lots = u64_at(self.data, node + 40)?;
            let expiry = u64_at(self.data, node + 48)?;
            let expiry_time = u64_at(self.data, node + 56)?;
            if lots == 0
                || (expiry != 0 && expiry < slot)
                || (expiry_time != 0 && expiry_time < time)
            {
                continue;
            }
            let maker = u64_at(self.data, node + 32)?;
            let maker_node = self.node(
                self.traders,
                u32::try_from(maker).map_err(|_| Error::Layout)?,
                self.seats,
                144,
            )?;
            if key(self.data, maker_node + 16)? == *wallet {
                return Err(Error::Unsupported);
            }
            let rate = price.checked_mul(tick).ok_or(Error::Arithmetic)?;
            if rate == 0 {
                return Err(Error::Layout);
            }
            let take = lots.min(base_budget).min(quote_budget / rate);
            let cost = take.checked_mul(rate).ok_or(Error::Arithmetic)?;
            matched_base = matched_base.checked_add(take).ok_or(Error::Arithmetic)?;
            matched_quote = matched_quote.checked_add(cost).ok_or(Error::Arithmetic)?;
            base_budget -= take;
            quote_budget -= cost;
            if take < lots {
                break;
            }
        }
        let fees = u64::try_from((u128::from(matched_quote) * u128::from(fee)).div_ceil(10_000))
            .map_err(|_| Error::Arithmetic)?
            .div_ceil(units);
        let base = matched_base
            .checked_mul(self.header.base_lot)
            .ok_or(Error::Arithmetic)?;
        let quote = if sell {
            (matched_quote / units).checked_sub(fees)
        } else {
            matched_quote.div_ceil(units).checked_add(fees)
        }
        .ok_or(Error::Arithmetic)?
        .checked_mul(self.header.quote_lot)
        .ok_or(Error::Arithmetic)?;
        let (spent, output) = if sell { (base, quote) } else { (quote, base) };
        if spent > input {
            return Err(Error::Arithmetic);
        }
        Ok(Fill {
            input: spent,
            output,
            encountered,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RaydiumPool {
    pub config: [u8; 32],
    pub vaults: [[u8; 32]; 2],
    pub mints: [[u8; 32]; 2],
    pub token_programs: [[u8; 32]; 2],
    pub observation: [u8; 32],
    pub excluded: [u64; 2],
    pub creator_mode: u8,
    pub creator_enabled: bool,
}
impl RaydiumPool {
    pub fn decode(data: &[u8], timestamp: u64) -> Result<Self> {
        if data.len() != 637 || data[..8] != [247, 237, 227, 245, 215, 195, 222, 70] {
            return Err(Error::Layout);
        }
        if data[329] & 4 != 0 || u64_at(data, 373)? > timestamp {
            return Err(Error::Status);
        }
        if data[389] > 2 || data[390] > 1 {
            return Err(Error::Layout);
        }
        let mut excluded = [0u64; 2];
        for (i, value) in excluded.iter_mut().enumerate() {
            *value = u64_at(data, 341 + 8 * i)?
                .checked_add(u64_at(data, 357 + 8 * i)?)
                .and_then(|x| x.checked_add(u64_at(data, 397 + 8 * i).ok()?))
                .ok_or(Error::Arithmetic)?;
        }
        Ok(Self {
            config: key(data, 8)?,
            vaults: [key(data, 72)?, key(data, 104)?],
            mints: [key(data, 168)?, key(data, 200)?],
            token_programs: [key(data, 232)?, key(data, 264)?],
            observation: key(data, 296)?,
            excluded,
            creator_mode: data[389],
            creator_enabled: data[390] == 1,
        })
    }
    pub fn quote(
        &self,
        config: &[u8],
        balances: [u64; 2],
        zero_for_one: bool,
        amount: u64,
    ) -> Result<u64> {
        if config.len() != 236 || config[..8] != [218, 244, 33, 104, 203, 203, 43, 111] {
            return Err(Error::Layout);
        }
        if amount == 0 || amount > MAX_INPUT {
            return Err(Error::Amount);
        }
        let trade = u64_at(config, 12)?;
        let creator = if self.creator_enabled {
            u64_at(config, 108)?
        } else {
            0
        };
        if trade.checked_add(creator).ok_or(Error::Arithmetic)? >= 1_000_000 {
            return Err(Error::Unsupported);
        }
        let i = usize::from(!zero_for_one);
        let o = 1 - i;
        let rin = balances[i]
            .checked_sub(self.excluded[i])
            .ok_or(Error::Arithmetic)?;
        let rout = balances[o]
            .checked_sub(self.excluded[o])
            .ok_or(Error::Arithmetic)?;
        if rin == 0 || rout == 0 {
            return Err(Error::Amount);
        }
        let on_input = self.creator_mode == 0 || self.creator_mode as usize == i + 1;
        let fee = ceil_fee(amount, trade + if on_input { creator } else { 0 })?;
        let net = amount.checked_sub(fee).ok_or(Error::Amount)?;
        let numerator = u128::from(net) * u128::from(rout);
        let gross = u64::try_from(numerator / (u128::from(rin) + u128::from(net)))
            .map_err(|_| Error::Arithmetic)?;
        let output = gross
            .checked_sub(if on_input {
                0
            } else {
                ceil_fee(gross, creator)?
            })
            .ok_or(Error::Arithmetic)?;
        if output == 0 {
            return Err(Error::Amount);
        }
        Ok(output)
    }
}
fn ceil_fee(amount: u64, rate: u64) -> Result<u64> {
    u64::try_from((u128::from(amount) * u128::from(rate)).div_ceil(1_000_000))
        .map_err(|_| Error::Arithmetic)
}
pub fn raydium_swap(amount: u64, min_out: u64) -> [u8; 24] {
    let mut data = [0u8; 24];
    data[..8].copy_from_slice(&[143, 190, 90, 218, 196, 30, 51, 222]);
    data[8..16].copy_from_slice(&amount.to_le_bytes());
    data[16..24].copy_from_slice(&min_out.to_le_bytes());
    data
}
