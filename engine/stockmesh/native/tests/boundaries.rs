use skew_native::{
    clmm::sqrt_tick,
    surface::{Knot, Surface},
    Error, TransferFee,
};
#[test]
fn transfer_fee_rounding_caps_and_full_fee_are_integer_exact() {
    assert_eq!(
        TransferFee {
            bps: 1,
            maximum: 100
        }
        .net(1),
        Ok(0)
    );
    assert_eq!(
        TransferFee {
            bps: 100,
            maximum: 3
        }
        .net(10000),
        Ok(9997)
    );
    assert_eq!(
        TransferFee {
            bps: 10000,
            maximum: u64::MAX
        }
        .net(u64::MAX),
        Ok(0)
    );
    assert_eq!(TransferFee::default().net(u64::MAX), Ok(u64::MAX));
}
#[test]
fn mint_fee_epoch_switch_is_not_a_cached_constant() {
    let mut mint = vec![0; 166];
    mint[45] = 1;
    mint[165] = 1;
    mint.extend_from_slice(&[1, 0, 108, 0]);
    let mut ext = [0; 108];
    ext[72..80].copy_from_slice(&1u64.to_le_bytes());
    ext[80..88].copy_from_slice(&10u64.to_le_bytes());
    ext[88..90].copy_from_slice(&5u16.to_le_bytes());
    ext[90..98].copy_from_slice(&9u64.to_le_bytes());
    ext[98..106].copy_from_slice(&20u64.to_le_bytes());
    ext[106..108].copy_from_slice(&25u16.to_le_bytes());
    mint.extend_from_slice(&ext);
    assert_eq!(TransferFee::decode(&mint, 8).unwrap().bps, 5);
    assert_eq!(TransferFee::decode(&mint, 9).unwrap().bps, 25);
    for n in 0..mint.len() {
        if n != 82 && n != 166 {
            assert!(TransferFee::decode(&mint[..n], 9).is_err());
        }
    }
}

#[test]
fn confidential_fee_mint_config_does_not_hide_public_transfer_fees() {
    let mut mint = vec![0; 166];
    mint[45] = 1;
    mint[165] = 1;
    mint.extend_from_slice(&16u16.to_le_bytes());
    mint.extend_from_slice(&129u16.to_le_bytes());
    mint.extend_from_slice(&[0; 129]);
    assert_eq!(TransferFee::decode(&mint, 0), Ok(TransferFee::default()));

    mint[168..170].copy_from_slice(&128u16.to_le_bytes());
    assert_eq!(TransferFee::decode(&mint, 0), Err(Error::Unsupported));
}
#[test]
fn sampled_surface_never_extrapolates_or_crosses_generations() {
    let mut s = Surface::new(10);
    s.push(Knot {
        input: 100,
        output: 90,
        cu: 10,
    })
    .unwrap();
    s.push(Knot {
        input: 200,
        output: 170,
        cu: 20,
    })
    .unwrap();
    // Both output and CU are interpolated proposals; exact endpoints are unchanged.
    assert_eq!(s.estimate(150, 10), Ok((130, 15)));
    assert_eq!(s.estimate(200, 10), Ok((170, 20)));
    assert_eq!(s.estimate(201, 10), Err(Error::Capacity));
    assert_eq!(s.estimate(100, 11), Err(Error::Stale));
    assert_eq!(
        s.push(Knot {
            input: 300,
            output: 160,
            cu: 10
        }),
        Err(Error::Unsupported)
    );
}
#[test]
fn tick_mapping_is_monotone_and_respects_domain() {
    assert_eq!(sqrt_tick(0), Ok(1u128 << 64));
    assert_eq!(sqrt_tick(-443636), Ok(4295048016));
    assert_eq!(sqrt_tick(443636), Ok(79226673521066979257578248091));
    assert!(sqrt_tick(i32::MIN).is_err());
    let mut old = sqrt_tick(-443636).unwrap();
    for t in (-443635..=443636).step_by(97) {
        let next = sqrt_tick(t).unwrap();
        assert!(next > old);
        old = next;
    }
}
#[test]
fn adaptive_refinement_focuses_curvature_without_changing_sample_truth() {
    use skew_native::surface::{Knot, Surface};
    let mut s = Surface::new(9);
    for (input, output) in [(10, 100), (20, 180), (100, 500)] {
        s.push(Knot {
            input,
            output,
            cu: 10,
        })
        .unwrap();
    }
    assert_eq!(s.refinement_input(), Some(60));
    s.insert_refinement(Knot {
        input: 60,
        output: 400,
        cu: 20,
    })
    .unwrap();
    assert_eq!(s.estimate(60, 9).unwrap(), (400, 20));
    assert!(s
        .insert_refinement(Knot {
            input: 60,
            output: 401,
            cu: 20
        })
        .is_err());
    assert!(s
        .insert_refinement(Knot {
            input: 80,
            output: 399,
            cu: 20
        })
        .is_err());
    assert!(s.estimate(101, 9).is_err());
}
