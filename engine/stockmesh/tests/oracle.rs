use skew_engine::{optimizer::oracle::refine, Error};

#[test]
fn concave_integer_curves_match_exhaustive_reference() {
    for q in 1u64..80 {
        let quote = |i: usize, x: u64| Ok((500 + i as u64 * 3) * x - (1 + i as u64) * x * x);
        let plan = refine(3, q, 20_000, quote).unwrap();
        let mut exact = 0;
        for a in 0..=q {
            for b in 0..=q - a {
                exact = exact.max(
                    quote(0, a).unwrap() + quote(1, b).unwrap() + quote(2, q - a - b).unwrap(),
                );
            }
        }
        assert_eq!(plan.output, exact);
        assert_eq!(plan.inputs.iter().sum::<u64>(), q);
        assert!(!plan.exhausted);
    }
}

#[test]
fn oracle_capacity_failures_and_budget_preserve_feasible_incumbent() {
    let plan = refine(5, 100, 180, |i, x| {
        if x > 40 {
            Err(Error::Capacity)
        } else {
            Ok(x * (10 + i as u64))
        }
    })
    .unwrap();
    assert_eq!(plan.inputs.iter().sum::<u64>(), 100);
    assert!(plan.inputs.iter().all(|x| *x <= 40));
    assert!(plan.inputs.iter().filter(|x| **x > 0).count() <= 4);
    assert!(plan.oracle_calls <= 180);
    assert!(plan.output >= 1_000);
    assert_eq!(refine(2, 100, 0, |_, _| Ok(1)), Err(Error::WorkLimit));
}

#[test]
fn compute_budget_changes_the_executable_allocation() {
    let plan = skew_engine::optimizer::oracle::refine_bounded(2, 100, 1000, 120, |i, q| {
        Ok((
            q * if i == 0 { 10 } else { 9 },
            q * if i == 0 { 3 } else { 1 },
        ))
    })
    .unwrap();
    assert_eq!(plan.inputs[0], 10);
    assert_eq!(plan.inputs[1], 90);
    assert_eq!(plan.cost, 120);
    assert_eq!(plan.output, 910);
}
