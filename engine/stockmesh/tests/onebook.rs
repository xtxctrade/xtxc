use skew_engine::{
    onebook::{
        compile_virtual_book, match_continuous, ExecutableSlice, SettlementOptionalOrder, Side,
    },
    WorkMeter, SCALE,
};

fn order(
    side: Side,
    owner: u8,
    sequence: u64,
    claim: u8,
    price: u64,
    shares: u64,
) -> SettlementOptionalOrder {
    let mut claims = [[0; 32]; 8];
    claims[0] = [claim; 32];
    SettlementOptionalOrder {
        owner: [owner; 32],
        nonce: sequence,
        instrument: [9; 32],
        side,
        exposure_q32: shares * SCALE as u64,
        limit_price_q32: price * SCALE as u64,
        acceptable_claims: claims,
        claim_count: 1,
        delivered_claim: if side == Side::Sell {
            [claim; 32]
        } else {
            [0; 32]
        },
        max_conversion_bps: 0,
        expires_at_slot: 110,
        arrival_sequence: sequence,
    }
}

#[test]
fn one_bid_crosses_multiple_issuer_claims_without_an_epoch() {
    let mut buyer = order(Side::Buy, 1, 1, 3, 221, 10);
    buyer.acceptable_claims[1] = [4; 32];
    buyer.claim_count = 2;
    let orders = [
        buyer,
        order(Side::Sell, 2, 2, 3, 219, 4),
        order(Side::Sell, 3, 3, 4, 220, 8),
    ];
    let report = match_continuous(&orders, 100, &mut WorkMeter::new(1_000)).unwrap();
    assert_eq!(report.fill_count, 2);
    assert_eq!(report.filled_exposure_q32[0], 10 * SCALE as u64);
    assert_eq!(report.residual_exposure_q32[2], 2 * SCALE as u64);
    assert_eq!(report.fills[0].claim, [3; 32]);
    assert_eq!(report.fills[1].claim, [4; 32]);
}

#[test]
fn unacceptable_claim_or_conversion_bound_stays_residual() {
    let mut buyer = order(Side::Buy, 1, 1, 3, 220, 5);
    buyer.max_conversion_bps = 100;
    let orders = [buyer, order(Side::Sell, 2, 2, 4, 200, 5)];
    let report = match_continuous(&orders, 100, &mut WorkMeter::new(1_000)).unwrap();
    assert_eq!(report.fill_count, 0);

    let orders = [buyer, order(Side::Sell, 2, 2, 3, 217, 5)];
    let report = match_continuous(&orders, 100, &mut WorkMeter::new(1_000)).unwrap();
    assert_eq!(report.fill_count, 1);

    let orders = [buyer, order(Side::Sell, 2, 2, 3, 220, 5)];
    let report = match_continuous(&orders, 100, &mut WorkMeter::new(1_000)).unwrap();
    assert_eq!(report.fill_count, 0);
}

#[test]
fn virtual_depth_is_price_sorted_and_generation_pinned() {
    let slices = [
        ExecutableSlice {
            claim: [3; 32],
            source: [8; 32],
            state_hash: [7; 32],
            input_usdc_atoms: 220_000_000,
            exposure_q32: SCALE as u64,
            conversion_bps: 0,
            expires_at_slot: 110,
        },
        ExecutableSlice {
            claim: [4; 32],
            source: [6; 32],
            state_hash: [7; 32],
            input_usdc_atoms: 219_000_000,
            exposure_q32: SCALE as u64,
            conversion_bps: 0,
            expires_at_slot: 110,
        },
    ];
    let book = compile_virtual_book(&slices, 100, &mut WorkMeter::new(100)).unwrap();
    assert_eq!(book.level_count, 2);
    assert_eq!(book.levels[0].claim, [4; 32]);
    assert!(book.levels[0].price_q32 < book.levels[1].price_q32);

    let mut incoherent = slices;
    incoherent[1].state_hash = [5; 32];
    assert!(compile_virtual_book(&incoherent, 100, &mut WorkMeter::new(100)).is_err());
}
