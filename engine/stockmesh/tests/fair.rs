use skew_engine::{
    fair::{self, Band, Interval, Side},
    SCALE,
};
fn interval(group: u16, low: u64, high: u64) -> Interval {
    Interval { group, low, high }
}
#[test]
fn consensus_rejects_sybil_votes_disconnected_regions_and_wide_uncertainty() {
    let xs = [
        interval(1, 100, 110),
        interval(2, 103, 113),
        interval(3, 900, 910),
    ];
    assert_eq!(fair::consensus(&xs, 1, 2000).unwrap().low, 103);
    let mut sybil = xs;
    sybil[2].group = 1;
    assert!(fair::consensus(&sybil, 1, 2000).is_err());
    assert!(fair::consensus(
        &[
            interval(1, 100, 110),
            interval(2, 100, 102),
            interval(3, 108, 110)
        ],
        1,
        2000
    )
    .is_err());
    assert!(fair::consensus(
        &[
            interval(1, 100, 200),
            interval(2, 100, 200),
            interval(3, 100, 200)
        ],
        1,
        2000
    )
    .is_err());
    assert!(fair::consensus(&xs[..2], 1, 2000).is_err());
    assert!(fair::consensus(&xs, 2, 2000).is_err());
}
#[test]
fn limits_can_only_tighten_and_crossing_conserves_both_assets() {
    let band = Band {
        low: 2 * SCALE as u64,
        high: 2 * SCALE as u64,
        groups: 3,
        quorum: 2,
    };
    assert_eq!(band.min_output(100, Side::BuyBase, 1).unwrap(), 50);
    assert_eq!(band.min_output(100, Side::BuyBase, 60).unwrap(), 60);
    assert_eq!(band.min_output(100, Side::SellBase, 1).unwrap(), 200);
    let c = fair::cross(band, 200, 100, 60, 120).unwrap();
    assert_eq!(
        (
            c.base,
            c.quote,
            c.buyer_quote_residual,
            c.seller_base_residual
        ),
        (60, 120, 80, 0)
    );
    assert!(fair::cross(band, 200, 101, 60, 120).is_err());
    assert!(fair::cross(band, 200, 100, 60, 121).is_err());
    for buy in 1..1000 {
        if let Ok(c) = fair::cross(band, buy, 1, 700, 1) {
            assert_eq!(c.quote + c.buyer_quote_residual, buy);
            assert_eq!(c.base + c.seller_base_residual, 700);
            assert!(u128::from(c.quote) * SCALE >= u128::from(c.base) * u128::from(band.low));
        }
    }
}

#[test]
fn quantity_rounding_cannot_escape_a_narrow_band() {
    let p = 3 * SCALE as u64 / 2;
    let band = Band {
        low: p,
        high: p,
        groups: 3,
        quorum: 2,
    };
    assert!(fair::cross(band, 100, 1, 1, 1).is_err());
    let c = fair::cross(band, 100, 1, 2, 1).unwrap();
    assert_eq!((c.base, c.quote), (2, 3));
    assert!(fair::consensus(
        &[interval(1, 1, 1), interval(2, 1, 1), interval(3, 1, 1)],
        usize::MAX,
        100
    )
    .is_err());
}
