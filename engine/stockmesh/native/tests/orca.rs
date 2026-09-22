use skew_native::{
    orca::{sqrt_tick, OrcaCurve},
    TransferFee,
};

fn fixture() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut pool = vec![0; 653];
    pool[..8].copy_from_slice(&[63, 149, 209, 12, 225, 128, 99, 9]);
    pool[41..43].copy_from_slice(&1u16.to_le_bytes());
    pool[43..45].copy_from_slice(&1u16.to_le_bytes());
    pool[45..47].copy_from_slice(&3000u16.to_le_bytes());
    pool[49..65].copy_from_slice(&1_000_000_000_000u128.to_le_bytes());
    pool[65..81].copy_from_slice(&sqrt_tick(10).unwrap().to_le_bytes());
    pool[81..85].copy_from_slice(&10i32.to_le_bytes());
    let mut fixed = vec![0; 9988];
    fixed[..8].copy_from_slice(&[69, 97, 189, 190, 110, 7, 66, 187]);
    fixed[9956..].copy_from_slice(&[7; 32]);
    let mut dynamic = vec![0; 148];
    dynamic[..8].copy_from_slice(&[17, 216, 246, 142, 225, 199, 218, 56]);
    dynamic[12..44].copy_from_slice(&[7; 32]);
    (pool, fixed, dynamic)
}

fn decode(p: &[u8], a: &[&[u8]]) -> skew_native::Result<OrcaCurve> {
    OrcaCurve::decode([7; 32], p, a, None, [TransferFee::default(); 2], 100, true)
}

#[test]
fn static_and_dynamic_layouts_have_identical_integer_quotes() {
    let (p, f, d) = fixture();
    let fixed = decode(&p, &[&f]).unwrap();
    let dynamic = decode(&p, &[&d]).unwrap();
    for input in [100, 1000, 10_000, 1_000_000, 100_000_000] {
        let quote = fixed.quote(input).unwrap();
        assert!(quote > 0 && quote < input);
        assert_eq!(quote, dynamic.quote(input).unwrap());
    }
    assert!(fixed.quote(0).is_err());
    assert!(fixed.quote(u64::MAX).is_err());
    assert!(fixed.quote(1_000_000_000).is_err()); // Outside supplied arrays.
}

#[test]
fn malformed_arrays_never_enter_the_compiled_surface() {
    let (p, f, d) = fixture();
    for end in [0, 7, 8, 11, 12, 43, 44, 59, 60, 147] {
        assert!(decode(&p, &[&d[..end]]).is_err(), "truncation {end}");
    }
    for end in [0, 7, 41, 65, 81, 652] {
        assert!(decode(&p[..end], &[&f]).is_err());
    }
    assert!(decode(&p, &[]).is_err());
    assert!(decode(&p, &[&f, &f]).is_err());
    assert!(decode(&p, &[f.as_slice(); 7]).is_err());
    for (at, value) in [(12, 6), (44, 1), (55, 1), (60, 2), (4, 0)] {
        let mut bad = d.clone();
        bad[at] = value;
        assert!(decode(&p, &[&bad]).is_err(), "corruption {at}");
    }
    let mut trailing = d;
    trailing.push(0);
    assert!(decode(&p, &[&trailing]).is_err());
    let mut wrong_net = f.clone();
    wrong_net[12] = 1;
    wrong_net[13..29].copy_from_slice(&2i128.to_le_bytes());
    wrong_net[29..45].copy_from_slice(&1u128.to_le_bytes());
    assert!(decode(&p, &[&wrong_net]).is_err());
    let mut gap = f;
    gap[8..12].copy_from_slice(&176i32.to_le_bytes());
    assert!(decode(&p, &[&gap]).is_err());
}

#[test]
fn adaptive_oracle_is_bound_to_time_pool_and_known_layout() {
    let (mut p, f, _) = fixture();
    p[43..45].copy_from_slice(&2u16.to_le_bytes());
    assert!(decode(&p, &[&f]).is_err());
    let mut oracle = vec![0; 254];
    oracle[..8].copy_from_slice(&[139, 194, 131, 179, 140, 179, 229, 244]);
    oracle[8..40].copy_from_slice(&[7; 32]);
    oracle[48..50].copy_from_slice(&30u16.to_le_bytes());
    oracle[50..52].copy_from_slice(&600u16.to_le_bytes());
    oracle[54..58].copy_from_slice(&100u32.to_le_bytes());
    oracle[58..62].copy_from_slice(&100_000u32.to_le_bytes());
    oracle[62..64].copy_from_slice(&1u16.to_le_bytes());
    oracle[82..90].copy_from_slice(&100u64.to_le_bytes());
    oracle[90..98].copy_from_slice(&100u64.to_le_bytes());
    let run = |o: &[u8]| {
        OrcaCurve::decode(
            [7; 32],
            &p,
            &[&f],
            Some(o),
            [TransferFee::default(); 2],
            100,
            true,
        )
    };
    assert!(run(&oracle).unwrap().quote(1_000_000).is_ok());
    for (at, value) in [
        (8, 6),
        (40, 101),
        (62, 0),
        (82, 101),
        (90, 101),
        (126, 1),
        (253, 1),
    ] {
        let mut bad = oracle.clone();
        bad[at] = value;
        assert!(run(&bad).is_err(), "oracle corruption {at}");
    }
}
