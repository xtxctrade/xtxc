use skew_native::{damm::DammCurve, Error, TransferFee};
fn pool() -> Vec<u8> {
    let mut p = vec![0; 1112];
    p[..8].copy_from_slice(&[241, 154, 109, 4, 17, 177, 109, 188]);
    p[8..16].copy_from_slice(&10_000_000u64.to_le_bytes());
    for (o, x) in [
        (360, 1_000_000u128 << 64),
        (424, 1u128 << 63),
        (440, 2u128 << 64),
        (456, 1u128 << 64),
    ] {
        p[o..o + 16].copy_from_slice(&x.to_le_bytes());
    }
    p
}
fn quote(p: &[u8], q: u64, d: bool) -> skew_native::Result<u64> {
    DammCurve::decode(p, [TransferFee::default(); 2], 100, 100)?.quote(q, d)
}
#[test]
fn liquidity_scale_and_fee_direction_are_not_ray_clmm_semantics() {
    let mut p = pool();
    assert_eq!(quote(&p, 10000, true), Ok(9801));
    assert_eq!(quote(&p, 10000, false), Ok(9801));
    p[484] = 1;
    assert_eq!(quote(&p, 10000, false), Ok(9802));
    assert_eq!(quote(&p, 10000, true), Ok(9801));
    assert_eq!(quote(&p, 2_000_000, false), Err(Error::Capacity));
}
#[test]
fn activation_halt_compounding_and_invalid_layout_are_rejected() {
    let mut p = pool();
    p[481] = 1;
    assert!(quote(&p, 100, true).is_err());
    p[481] = 0;
    p[484] = 2;
    assert!(quote(&p, 100, true).is_err());
    p[484] = 0;
    p[472..480].copy_from_slice(&101u64.to_le_bytes());
    assert_eq!(quote(&p, 100, true), Err(Error::Capacity));
    for n in 0..1112 {
        assert!(quote(&p[..n], 100, true).is_err());
    }
}
#[test]
fn stored_volatility_changes_current_fee_and_linear_scheduler_uses_clock() {
    let mut p = pool();
    let base = quote(&p, 10000, true).unwrap();
    p[22..24].copy_from_slice(&10u16.to_le_bytes());
    p[24..32].copy_from_slice(&10u64.to_le_bytes());
    p[32..40].copy_from_slice(&1_000_000u64.to_le_bytes());
    assert!(quote(&p, 10000, true).unwrap() > base);
    p[56] = 1;
    p[64..68].copy_from_slice(&10_000_000u32.to_le_bytes());
    p[68..72].copy_from_slice(&1_000_000u32.to_le_bytes());
    p[72..74].copy_from_slice(&100u16.to_le_bytes());
    p[120..136].copy_from_slice(&10_000u128.to_le_bytes());
    assert_eq!(quote(&p, 10000, true), Ok(base));
}
