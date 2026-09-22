use skew_engine::clearing::*;
use skew_engine::{Error, WorkMeter};

fn o(id: u8, sell: u8, buy: u8, amount: u64, min_out: u64) -> FlowIntent {
    FlowIntent {
        owner: [id; 32],
        nonce: 1,
        sell,
        buy,
        amount,
        min_out,
        expires_at_slot: 100,
    }
}
fn run(orders: &[FlowIntent], prices: &[u64]) -> FoldReport {
    fold(
        orders,
        prices,
        100,
        MAX_PIVOTS,
        &mut WorkMeter::new(1_000_000),
    )
    .unwrap()
}

#[test]
fn bilateral_cross_and_three_asset_cycle_conserve_every_token() {
    let pair = [o(1, 0, 1, 100, 200), o(2, 1, 0, 120, 60)];
    let r = run(&pair, &[2, 1]);
    assert_eq!(
        r.fills[0],
        FoldFill {
            input: 60,
            output: 120
        }
    );
    assert_eq!(
        r.fills[1],
        FoldFill {
            input: 120,
            output: 60
        }
    );
    assert_eq!(residuals(&pair, &r).unwrap().groups[0].amount_in, 40);
    let tri = [o(1, 0, 1, 10, 20), o(2, 1, 2, 20, 40), o(3, 2, 0, 40, 10)];
    let r = run(&tri, &[4, 2, 1]);
    assert_eq!(r.requested_value, r.internally_cleared_value);
    assert_eq!(residuals(&tri, &r).unwrap().len, 0);
    assert!(r.optimal_at_fixed_prices);
}

#[test]
fn circulation_matches_exhaustive_optimum_including_overlapping_cycles() {
    let mut seed = 7743u64;
    let mut reverse_seen = false;
    for _ in 0..600 {
        let mut orders = Vec::new();
        for i in 0..6 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let a = ((seed >> 32) % 4) as u8;
            let b = (a + 1 + ((seed >> 40) % 3) as u8) % 4;
            let cap = 1 + ((seed >> 48) % 3);
            orders.push(o(i + 1, a, b, cap, cap));
        }
        let report = run(&orders, &[1, 1, 1, 1]);
        assert!(report.optimal_at_fixed_prices);
        reverse_seen |= report.reverse_edge_pivots > 0;
        let mut optimum = 0;
        for code in 0..4096u32 {
            let mut n = code;
            let mut net = [0i64; 4];
            let mut total = 0;
            let mut valid = true;
            for order in &orders {
                let x = (n % 4) as u64;
                n /= 4;
                if x > order.amount {
                    valid = false;
                    break;
                }
                net[order.sell as usize] -= x as i64;
                net[order.buy as usize] += x as i64;
                total += x as u128;
            }
            if valid && net == [0; 4] {
                optimum = optimum.max(total);
            }
        }
        assert_eq!(
            report.internally_cleared_value, optimum,
            "orders={orders:?}"
        );
    }
    assert!(
        reverse_seen,
        "corpus must exercise reversal of prior internal matches"
    );
}

#[test]
fn limits_price_quantization_and_exhausted_pivot_budget_are_explicit() {
    let orders = [o(1, 0, 1, 10, 30), o(2, 1, 0, 20, 10)];
    assert_eq!(run(&orders, &[2, 1]).internally_cleared_value, 0);
    let orders = [o(1, 0, 1, 10, 10), o(2, 1, 0, 10, 10)];
    let r = fold(&orders, &[1, 1], 100, 0, &mut WorkMeter::new(1000)).unwrap();
    assert_eq!(r.internally_cleared_value, 0);
    assert!(!r.optimal_at_fixed_prices);
    assert_eq!(
        fold(&orders, &[1, 0], 100, 10, &mut WorkMeter::new(1000)),
        Err(Error::InvalidAmount)
    );
    let r = run(&[o(1, 0, 1, 10, 0), o(2, 1, 0, 10, 0)], &[3, 7]);
    assert_eq!(r.value_quantum, 21);
    assert_eq!(r.fills[0].input, 7);
    assert_eq!(r.fills[1].input, 3);
}

#[test]
fn residual_distribution_enforces_each_users_minimum_and_dust_conservation() {
    let orders = [o(2, 0, 1, 100, 90), o(1, 0, 1, 200, 199)];
    let r = run(&orders, &[1, 1]);
    let g = residuals(&orders, &r).unwrap().groups[0];
    assert_eq!(g.min_out, 289);
    assert_eq!(distribute(&orders, &r, g, 288), Err(Error::MinOut));
    let out = distribute(&orders, &r, g, 299).unwrap();
    assert_eq!(out[0], 93);
    assert_eq!(out[1], 206);
    assert_eq!(out.iter().sum::<u64>(), 299);
}

#[test]
fn duplicate_and_expired_intents_are_rejected() {
    let a = o(1, 0, 1, 100, 100);
    assert_eq!(
        fold(&[a, a], &[1, 1], 100, 10, &mut WorkMeter::new(1000)),
        Err(Error::Duplicate)
    );
    assert_eq!(
        fold(&[a], &[1, 1], 101, 10, &mut WorkMeter::new(1000)),
        Err(Error::Expired)
    );
}

#[test]
fn asset_lots_clear_cycles_without_a_global_price_lcm() {
    // One lot is approximately one dollar for USDC, SOL and NVDAx. These lot
    // sizes conserve token atoms exactly while avoiding the LCM of live price
    // numerators.
    let lots = [1_000_000, 9_901_337, 5_336];
    let orders = [
        o(1, 1, 0, lots[1] * 10, lots[0] * 10),
        o(2, 0, 2, lots[0] * 10, lots[2] * 10),
        o(3, 2, 1, lots[2] * 10, lots[1] * 10),
    ];
    let report = fold_lots(
        &orders,
        &lots,
        100,
        MAX_PIVOTS,
        &mut WorkMeter::new(1_000_000),
    )
    .unwrap();
    assert!(report.optimal_at_fixed_lots);
    assert_eq!(report.requested_lots, 30);
    assert_eq!(report.internally_cleared_lots, 30);
    assert_eq!(report.fills[0].input, lots[1] * 10);
    assert_eq!(report.fills[1].output, lots[2] * 10);
    assert_eq!(report.fills[2].output, lots[1] * 10);
    assert_eq!(residuals_lots(&orders, &report).unwrap().len, 0);
}

#[test]
fn asset_lots_leave_dust_and_too_strict_limits_in_the_residual() {
    let lots = [1_000_000, 5_000];
    let orders = [
        o(1, 0, 1, 10_500_000, 50_000),
        o(2, 1, 0, 50_000, 10_000_000),
        // At the lot price 1 USDC buys 5,000 stock atoms. This owner demands
        // more, so the whole order must remain external.
        o(3, 0, 1, 2_000_000, 10_001),
    ];
    let report = fold_lots(
        &orders,
        &lots,
        100,
        MAX_PIVOTS,
        &mut WorkMeter::new(1_000_000),
    )
    .unwrap();
    assert_eq!(report.fills[0].input, 10_000_000);
    assert_eq!(report.fills[1].output, 10_000_000);
    assert_eq!(report.fills[2], FoldFill::default());
    let residual = residuals_lots(&orders, &report).unwrap();
    assert_eq!(residual.len, 1);
    assert_eq!(residual.groups[0].amount_in, 2_500_000);
    assert_eq!(residual.groups[0].min_out, 10_001);
    assert_eq!(residual.groups[0].members, 0b101);
}
