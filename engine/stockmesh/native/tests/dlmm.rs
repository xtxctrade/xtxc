use skew_native::{dlmm::DlmmCurve, Error, TransferFee};
fn fixture() -> (Vec<u8>, Vec<u8>) {
    let mut p = vec![0; 904];
    p[..8].copy_from_slice(&[33, 11, 49, 98, 181, 101, 177, 13]);
    p[24..28].copy_from_slice(&(-1000i32).to_le_bytes());
    p[28..32].copy_from_slice(&1000i32.to_le_bytes());
    p[80..82].copy_from_slice(&100u16.to_le_bytes());
    let mut a = vec![0; 10136];
    a[..8].copy_from_slice(&[92, 142, 92, 220, 5, 148, 70, 181]);
    a[24..56].copy_from_slice(&[7; 32]);
    for o in [56, 200] {
        a[o..o + 8].copy_from_slice(&100u64.to_le_bytes());
        a[o + 8..o + 16].copy_from_slice(&100u64.to_le_bytes());
        a[o + 16..o + 32].copy_from_slice(&(1u128 << 64).to_le_bytes());
    }
    (p, a)
}
fn curve(p: &[u8], a: &[u8], time: u64) -> skew_native::Result<DlmmCurve> {
    DlmmCurve::decode([7; 32], p, &[a], [TransferFee::default(); 2], 100, time)
}
#[test]
fn bin_crossing_charges_fees_per_bin_and_never_silently_partial_fills() {
    let (mut p, a) = fixture();
    p[8..10].copy_from_slice(&10000u16.to_le_bytes());
    let c = curve(&p, &a, 100).unwrap();
    assert_eq!(c.quote(150, false), Ok(147));
    assert_eq!(c.quote(203, false), Ok(199));
    assert_eq!(c.quote(205, false), Err(Error::Capacity));
    assert_eq!(c.quote(0, false), Err(Error::Capacity));
}
#[test]
fn only_y_fee_changes_rounding_in_x_to_y_direction() {
    let (mut p, mut a) = fixture();
    p[8..10].copy_from_slice(&10000u16.to_le_bytes());
    a[64..72].copy_from_slice(&1000u64.to_le_bytes());
    a[72..88].copy_from_slice(&(2u128 << 64).to_le_bytes());
    assert_eq!(curve(&p, &a, 100).unwrap().quote(101, true), Ok(198));
    p[36] = 1;
    assert_eq!(curve(&p, &a, 100).unwrap().quote(101, true), Ok(199));
    assert_eq!(curve(&p, &a, 100).unwrap().quote(101, false), Ok(49));
}
#[test]
fn processed_and_open_orders_are_directional_and_bounded() {
    let (mut p, mut a) = fixture();
    p[35] = 2;
    a[56..72].fill(0);
    a[64..72].copy_from_slice(&3u64.to_le_bytes());
    a[56 + 128..56 + 136].copy_from_slice(&2u64.to_le_bytes());
    a[56 + 112..56 + 120].copy_from_slice(&4u64.to_le_bytes());
    let c = curve(&p, &a, 100).unwrap();
    assert_eq!(c.quote(9, true), Ok(9));
    assert_eq!(c.quote(10, true), Err(Error::Capacity));
    // Opposite direction skips ask-side order liquidity and consumes the next MM bin.
    assert_eq!(c.quote(100, false), Ok(100));
    assert_eq!(c.quote(101, false), Err(Error::Capacity));
}

#[test]
fn undetermined_pool_supports_orders_only_without_reward_mints() {
    let (mut p, mut a) = fixture();
    p[35] = 0;
    a[56..72].fill(0);
    a[200..216].fill(0);
    a[56 + 112..56 + 120].copy_from_slice(&4u64.to_le_bytes());
    a[56 + 140] = 1;
    assert_eq!(curve(&p, &a, 100).unwrap().quote(4, false), Ok(4));

    // RewardInfo[0].mint makes FunctionType::Undetermined a liquidity-mining
    // pool, so the exact same order fields are not executable liquidity.
    p[264] = 1;
    assert_eq!(
        curve(&p, &a, 100).unwrap().quote(4, false),
        Err(Error::Capacity)
    );
}
#[test]
fn activation_time_and_snapshot_time_are_enforced() {
    let (mut p, a) = fixture();
    p[56..64].copy_from_slice(&100u64.to_le_bytes());
    assert!(matches!(curve(&p, &a, 99), Err(Error::Stale)));
    p[75] = 1;
    p[86] = 1;
    p[816..824].copy_from_slice(&102u64.to_le_bytes());
    assert!(matches!(curve(&p, &a, 101), Err(Error::Capacity)));
    assert!(curve(&p, &a, 102).is_ok());
}
#[test]
fn malformed_and_disconnected_accounts_fail_closed_without_panics() {
    let (p, a) = fixture();
    for n in 0..904 {
        assert!(curve(&p[..n], &a, 100).is_err());
    }
    for n in (0..10136).step_by(31) {
        assert!(curve(&p, &a[..n], 100).is_err());
    }
    let mut b = a.clone();
    b[24] = 8;
    assert!(curve(&p, &b, 100).is_err());
    b = a.clone();
    b[8..16].copy_from_slice(&30_678_336i64.to_le_bytes());
    assert!(DlmmCurve::decode(
        [7; 32],
        &p,
        &[&a, &b],
        [TransferFee::default(); 2],
        100,
        100
    )
    .is_err());
}
