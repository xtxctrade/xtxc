use skew_engine::clearing::*;
use skew_engine::reflow::{Intent, Limits};
use skew_engine::runtime::fixture::{book, rate, FixtureHost};
use skew_engine::runtime::EdgePin;
use skew_engine::{Error, WorkMeter};

#[test]
fn fold_then_residual_reflow_then_verify_all_user_and_token_postconditions() {
    let orders = [
        FlowIntent {
            owner: [1; 32],
            nonce: 1,
            sell: 0,
            buy: 1,
            amount: 100,
            min_out: 190,
            expires_at_slot: 100,
        },
        FlowIntent {
            owner: [2; 32],
            nonce: 1,
            sell: 1,
            buy: 0,
            amount: 120,
            min_out: 60,
            expires_at_slot: 100,
        },
    ];
    let folded = fold(&orders, &[2, 1], 100, 256, &mut WorkMeter::new(100_000)).unwrap();
    let groups = residuals(&orders, &folded).unwrap();
    assert_eq!(groups.len, 1);
    let group = groups.groups[0];
    assert_eq!(group.amount_in, 40);
    let venues = [book(10, &[(40, rate(2, 1))], 100)];
    let mut host = FixtureHost::new(&venues, 40, 100).unwrap();
    let intent = Intent {
        input_mint: [1; 32],
        output_mint: [2; 32],
        amount_in: 40,
        min_out: group.min_out,
        expires_at_slot: 100,
        max_snapshot_age: 0,
    };
    let receipt = host
        .atomic(&[EdgePin::from(venues[0])], intent, Limits::default())
        .unwrap();
    let residual_credits = distribute(&orders, &folded, group, receipt.output_received).unwrap();
    let credits = [
        folded.fills[0].output + residual_credits[0],
        folded.fills[1].output + residual_credits[1],
    ];
    let mut external = [0i128; MAX_ASSETS];
    external[0] = -i128::from(receipt.input_spent);
    external[1] = i128::from(receipt.output_received);
    verify_settlement(&orders, &[100, 120], &credits, external).unwrap();
    assert_eq!(credits, [200, 60]);
    external[1] += 1;
    assert_eq!(
        verify_settlement(&orders, &[100, 120], &credits, external),
        Err(Error::BalanceInvariant)
    );
}

#[test]
fn forged_internal_credit_and_user_limit_violation_are_rejected() {
    let orders = [FlowIntent {
        owner: [1; 32],
        nonce: 1,
        sell: 0,
        buy: 1,
        amount: 100,
        min_out: 100,
        expires_at_slot: 100,
    }];
    let mut folded = FoldReport::default();
    folded.fills[0] = FoldFill {
        input: 10,
        output: 100,
    };
    assert_eq!(residuals(&orders, &folded), Err(Error::BalanceInvariant));
    assert_eq!(
        verify_settlement(&orders, &[100], &[99], [0; MAX_ASSETS]),
        Err(Error::MinOut)
    );
}
