use skew_engine::compiler::{compile, Curve};
use skew_engine::optimizer::{search_reference, search_tape, tape::ResidualTape, waterfill};
use skew_engine::runtime::fixture::{book, rate};
use skew_engine::{Error, WorkMeter};

fn meter() -> WorkMeter {
    WorkMeter::new(1_000_000)
}

#[test]
fn exhausted_frontier_is_not_a_zero_rate_band_and_ties_are_stable() {
    for edges in 1..=8usize {
        let snapshots: Vec<_> = (0..edges)
            .map(|i| book(10 + i as u8, &[(3, rate(1, 2)), (5 + i as u64, 0)], 100))
            .collect();
        for mask in 1..(1u16 << edges) {
            // Excluded storage deliberately contains invalid/default curves.
            let mut curves = vec![Curve::default(); edges];
            let mut capacity = 0;
            for (i, s) in snapshots.iter().enumerate() {
                if mask & (1 << i) != 0 {
                    curves[i] = compile(s, 1000, &mut meter()).unwrap();
                    capacity += s.capacity;
                }
            }
            let tape = ResidualTape::build(&curves, mask as u8, &mut meter()).unwrap();
            let mut subset = mask;
            while subset != 0 {
                for amount in [1, 3, 7, capacity, capacity + 1] {
                    assert_eq!(
                        tape.allocate(amount, subset as u8, &mut meter()),
                        waterfill(&curves, amount, subset as u8, &mut meter()),
                        "edges={edges}, built={mask}, selected={subset}, amount={amount}"
                    );
                }
                subset = (subset - 1) & mask;
            }
        }
    }
}

#[test]
fn conservative_indexed_scores_follow_current_envelopes_and_per_venue_floors() {
    let s: Vec<_> = (0..8)
        .map(|i| book(10 + i, &[(7, rate(9, 7)), (11, rate(1, 2))], 100))
        .collect();
    let mut c: Vec<_> = s
        .iter()
        .map(|v| compile(v, 100, &mut meter()).unwrap())
        .collect();
    let tape = ResidualTape::build(&c, 255, &mut meter()).unwrap();
    for error in [0, 1, 3, 100, u64::MAX] {
        for (i, v) in c.iter_mut().enumerate() {
            v.lower_error_atoms = if i % 2 == 0 { error } else { 0 };
        }
        for mask in 1..=255 {
            for amount in [1, 2, 7, 11, 29, 60] {
                for legs in [1, 2, 4] {
                    assert_eq!(
                        search_tape(&s, &c, &tape, amount, mask, legs, &mut meter()),
                        search_reference(&s, &c, amount, mask, legs, &mut meter()),
                        "error={error}, mask={mask}, amount={amount}, legs={legs}"
                    );
                }
            }
        }
    }
}

#[test]
fn optimized_work_remains_budgeted_and_workspace_does_not_grow() {
    let s = [book(10, &[(100, rate(2, 1))], 100)];
    let c = [compile(&s[0], 100, &mut meter()).unwrap()];
    assert!(matches!(
        ResidualTape::build(&c, 1, &mut WorkMeter::new(0)),
        Err(Error::WorkLimit)
    ));
    let tape = ResidualTape::build(&c, 1, &mut meter()).unwrap();
    let mut unrestricted = meter();
    let expected = tape.allocate(99, 1, &mut unrestricted).unwrap();
    for limit in 0..=unrestricted.used {
        let mut bounded = WorkMeter::new(limit);
        let result = tape.allocate(99, 1, &mut bounded);
        assert!(bounded.used <= limit);
        if limit < unrestricted.used {
            assert_eq!(result, Err(Error::WorkLimit));
        } else {
            assert_eq!(result, Ok(expected));
        }
    }
    assert!(std::mem::size_of::<ResidualTape>() <= 12_640);
}
