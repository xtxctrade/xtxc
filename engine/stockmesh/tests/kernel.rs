use skew_engine::compiler::{compile, Curve, Model};
use skew_engine::optimizer::tape::ResidualTape;
use skew_engine::optimizer::{search, search_reference, waterfill};
use skew_engine::reflow::{Intent, Limits};
use skew_engine::runtime::fixture::{book, rate, FixtureHost};
use skew_engine::runtime::{EdgePin, ExecutionHost};
use skew_engine::{Error, WorkMeter, MAX_ATOMS, MAX_EDGES};

fn meter() -> WorkMeter {
    WorkMeter::new(20_000_000)
}

#[test]
fn tape_binding_and_optimizer_asset_domain_are_checked() {
    let mut snapshots = [
        book(10, &[(100, rate(2, 1))], 100),
        book(11, &[(100, rate(1, 1))], 100),
    ];
    let mut curves: Vec<_> = snapshots
        .iter()
        .map(|s| compile(s, 100, &mut meter()).unwrap())
        .collect();
    let tape = ResidualTape::build(&curves, 3, &mut meter()).unwrap();
    curves[0].segments[0].marginal_q32 = rate(1, 1);
    assert_eq!(
        skew_engine::optimizer::search_tape(&snapshots, &curves, &tape, 100, 3, 2, &mut meter()),
        Err(Error::Identity)
    );
    curves[0] = compile(&snapshots[0], 100, &mut meter()).unwrap();
    snapshots[1].output_mint = [99; 32];
    assert_eq!(
        search(&snapshots, &curves, 100, 3, 2, &mut meter()),
        Err(Error::Identity)
    );
}

#[test]
fn changed_unexecuted_state_invalidates_and_rebuilds_the_tape() {
    struct Changing(FixtureHost);
    impl ExecutionHost for Changing {
        fn slot(&self) -> u64 {
            self.0.slot()
        }
        fn snapshot(&self, i: usize) -> skew_engine::Result<skew_engine::compiler::Snapshot> {
            self.0.snapshot(i)
        }
        fn balances(&self) -> skew_engine::Result<skew_engine::runtime::Balances> {
            self.0.balances()
        }
        fn invoke(&mut self, i: usize, amount: u64) -> skew_engine::Result<()> {
            self.0.invoke(i, amount)?;
            if i == 0 {
                self.0.venues[1].model = book(11, &[(60, rate(20, 1))], 100).model;
                self.0.venues[1].generation += 1;
            }
            Ok(())
        }
    }
    let (host, pins, intent) = partial_fixture();
    // Deliberately adversarial host: this is cache invalidation coverage, not a
    // claim that external transactions can interleave with locked SVM accounts.
    let receipt =
        skew_engine::reflow::execute(&mut Changing(host), &pins, intent, Limits::default())
            .unwrap();
    assert_eq!(receipt.tape_builds, 2);
    assert_eq!(receipt.curves_compiled, 4);
    assert_eq!(receipt.output_received, 1560);
}

#[test]
fn constant_product_chords_enclose_exact_integer_arithmetic() {
    for r in [1, 17, 1000, 10_000_000, MAX_ATOMS] {
        for y in [1, 9, 1000, 91_234] {
            if y / r > 60_000 {
                continue;
            }
            for fee in [0, 1, 3000, 100_000, 999_999] {
                let mut s = book(10, &[(10_000, rate(1, 1))], 100);
                s.model = Model::ConstantProduct {
                    reserve_in: r,
                    reserve_out: y,
                    fee_ppm: fee,
                };
                let curve = compile(&s, 10_000, &mut meter()).unwrap();
                for x in (0..=10_000).step_by(13).chain([1, 9999, 10_000]) {
                    let actual = s.exact_quote(x).unwrap();
                    assert!(
                        curve.lower_quote(x).unwrap() <= actual,
                        "lower r={r} y={y} fee={fee} x={x}"
                    );
                    assert!(
                        actual <= curve.quote(x).unwrap() + curve.upper_error_atoms,
                        "upper r={r} y={y} fee={fee} x={x}"
                    );
                }
            }
        }
    }
}

#[test]
fn residual_tape_matches_streaming_for_every_mask_and_residual() {
    let snapshots: Vec<_> = (0..8)
        .map(|i| {
            book(
                10 + i,
                &[
                    (11 + i as u64, rate(100 - i as u64, 7)),
                    (23, rate(80 - i as u64, 7)),
                    (31, rate(60, 7)),
                ],
                100,
            )
        })
        .collect();
    let curves: Vec<_> = snapshots
        .iter()
        .map(|s| compile(s, 1000, &mut meter()).unwrap())
        .collect();
    let tape = ResidualTape::build(&curves, 255, &mut meter()).unwrap();
    for mask in 1..=255 {
        for residual in 1..=550 {
            assert_eq!(
                tape.allocate(residual, mask, &mut meter()),
                waterfill(&curves, residual, mask, &mut meter()),
                "mask={mask} Q={residual}"
            );
        }
    }
}

#[test]
fn certified_cardinality_search_contains_bruteforce_integer_optimum() {
    for capacity in [3, 7, 10] {
        let snapshots = [
            book(10, &[(capacity, rate(23, 7))], 100),
            book(11, &[(capacity, rate(22, 7))], 100),
            book(12, &[(capacity, rate(21, 7))], 100),
        ];
        let curves: Vec<_> = snapshots
            .iter()
            .map(|s| compile(s, 30, &mut meter()).unwrap())
            .collect();
        for amount in 1..=capacity * 2 {
            let result = search(&snapshots, &curves, amount, 7, 2, &mut meter()).unwrap();
            assert_eq!(
                result,
                search_reference(&snapshots, &curves, amount, 7, 2, &mut meter()).unwrap()
            );
            let mut optimum = 0;
            for a in 0..=capacity {
                for b in 0..=capacity {
                    if a + b > amount {
                        continue;
                    }
                    let c = amount - a - b;
                    if c > capacity || [a, b, c].iter().filter(|x| **x > 0).count() > 2 {
                        continue;
                    }
                    let out = snapshots[0].exact_quote(a).unwrap()
                        + snapshots[1].exact_quote(b).unwrap()
                        + snapshots[2].exact_quote(c).unwrap();
                    optimum = optimum.max(out);
                }
            }
            assert!(result.plan.output <= optimum);
            assert!(optimum <= result.certificate.upper_output);
            assert_eq!(result.plan.inputs.iter().sum::<u64>(), amount);
        }
    }
}

fn partial_fixture() -> (FixtureHost, Vec<EdgePin>, Intent) {
    let snapshots = [
        book(10, &[(60, rate(10, 1))], 100),
        book(11, &[(60, rate(9, 1))], 100),
        book(12, &[(100, rate(8, 1))], 100),
    ];
    let mut host = FixtureHost::new(&snapshots, 100, 100).unwrap();
    host.settlement_caps[0] = 20;
    let pins = snapshots.iter().copied().map(EdgePin::from).collect();
    let intent = Intent {
        input_mint: [1; 32],
        output_mint: [2; 32],
        amount_in: 100,
        min_out: 900,
        expires_at_slot: 101,
        max_snapshot_age: 1,
    };
    (host, pins, intent)
}

#[test]
fn observed_partial_fill_reuses_tape_and_reallocates_residual() {
    let (mut host, pins, intent) = partial_fixture();
    let receipt = host.atomic(&pins, intent, Limits::default()).unwrap();
    assert_eq!(receipt.output_received, 900);
    assert_eq!(receipt.legs, 3);
    assert_eq!(receipt.reflows, 2);
    assert_eq!(receipt.trace[0].requested, 60);
    assert_eq!(receipt.trace[0].spent, 20);
    assert_eq!(receipt.trace[1].spent, 60);
    assert_eq!(receipt.tape_builds, 1);
    assert_eq!(receipt.curves_compiled, 3);
    assert_eq!(host.balances.input, 0);
}

#[test]
fn minout_fatal_cpi_work_and_leg_failures_roll_back_native_state() {
    for case in 0..5 {
        let (mut host, pins, mut intent) = partial_fixture();
        let mut limits = Limits::default();
        let expected = match case {
            0 => {
                intent.min_out = 901;
                Error::MinOut
            }
            1 => {
                host.fatal = 2;
                Error::FatalCpi
            }
            2 => {
                limits.max_work = 1;
                Error::WorkLimit
            }
            3 => {
                limits.max_legs = 2;
                Error::MinOut
            }
            _ => {
                limits.max_reflows = 0;
                Error::MinOut
            }
        };
        let before = host;
        assert_eq!(
            host.atomic(&pins, intent, limits),
            Err(expected),
            "case={case}"
        );
        assert_eq!(before, host);
    }
}

#[test]
fn unavailable_expired_and_stale_venues_are_excluded_before_cpi() {
    for case in 0..3 {
        let (mut host, pins, mut intent) = partial_fixture();
        intent.min_out = 800;
        match case {
            0 => host.venues[0].enabled = false,
            1 => host.venues[0].expires_at_slot = 99,
            _ => host.venues[0].slot = 1,
        }
        host.fatal = 1;
        let receipt = host.atomic(&pins, intent, Limits::default()).unwrap();
        assert!(receipt.trace[..receipt.legs as usize]
            .iter()
            .all(|t| t.edge != 0));
    }
}

#[test]
fn wrong_identity_and_shared_liquidity_are_rejected() {
    let (mut host, mut pins, intent) = partial_fixture();
    pins[0].program = [99; 32];
    assert_eq!(
        host.atomic(&pins, intent, Limits::default()),
        Err(Error::Identity)
    );
    pins[0] = host.venues[0].into();
    pins[1].liquidity_id = pins[0].liquidity_id;
    assert_eq!(
        host.atomic(&pins, intent, Limits::default()),
        Err(Error::AliasedLiquidity)
    );
}

#[test]
fn invalid_curves_amounts_and_bounds_fail_closed() {
    let s = book(10, &[(10, rate(1, 1)), (20, rate(2, 1))], 100);
    assert_eq!(compile(&s, 30, &mut meter()), Err(Error::InvalidCurve));
    let s = book(10, &[(10, rate(1, 1))], 100);
    assert_eq!(compile(&s, 0, &mut meter()), Err(Error::InvalidAmount));
    assert!(ResidualTape::build(&[Curve::default(); MAX_EDGES], 255, &mut meter()).is_err());
}

#[test]
fn unexpected_balance_drain_is_fatal() {
    struct Bad(FixtureHost);
    impl ExecutionHost for Bad {
        fn slot(&self) -> u64 {
            self.0.slot()
        }
        fn snapshot(&self, i: usize) -> skew_engine::Result<skew_engine::compiler::Snapshot> {
            self.0.snapshot(i)
        }
        fn balances(&self) -> skew_engine::Result<skew_engine::runtime::Balances> {
            self.0.balances()
        }
        fn invoke(&mut self, _: usize, _: u64) -> skew_engine::Result<()> {
            self.0.balances.input = 0;
            self.0.balances.output = 1000;
            Ok(())
        }
    }
    let (host, pins, intent) = partial_fixture();
    assert_eq!(
        skew_engine::reflow::execute(&mut Bad(host), &pins, intent, Limits::default()),
        Err(Error::BalanceInvariant)
    );
}
