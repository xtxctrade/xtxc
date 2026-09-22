use skew_engine::optimizer::oracle::refine_bounded_legs;

fn exposure_curve(edge: usize, input: u64) -> skew_engine::Result<(u64, u64)> {
    let input_squared = input.checked_mul(input).ok_or(skew_engine::Error::Arithmetic)?;
    let exposure = match edge {
        // Issuer product A has the best first dollar but shallower capacity.
        0 => input
            .checked_mul(100)
            .and_then(|value| value.checked_sub(input_squared))
            .ok_or(skew_engine::Error::Capacity)?,
        // Issuer product B starts lower but remains cheaper at larger size.
        1 => input
            .checked_mul(90)
            .and_then(|value| value.checked_sub(input_squared / 2))
            .ok_or(skew_engine::Error::Capacity)?,
        _ => return Err(skew_engine::Error::Bounds),
    };
    if exposure == 0 {
        return Err(skew_engine::Error::Capacity);
    }
    Ok((exposure, 0))
}

#[test]
fn one_economic_intent_splits_across_issuer_products_in_one_exposure_unit() {
    let split = refine_bounded_legs(2, 60, 2_048, u64::MAX, 2, exposure_curve).unwrap();
    let single = refine_bounded_legs(2, 60, 2_048, u64::MAX, 1, exposure_curve).unwrap();

    assert_eq!(split.inputs[0] + split.inputs[1], 60);
    assert!(split.inputs[0] > 0 && split.inputs[1] > 0);
    assert!(split.output > single.output);
    assert_eq!(single.inputs.iter().filter(|amount| **amount > 0).count(), 1);
}
