use skew_native::{memo::QuoteMemo, Error};
#[test]
fn bank_and_edge_identity_prevent_stale_price_or_failure_reuse() {
    let mut m = QuoteMemo::default();
    assert_eq!(m.quote(0, 100, || Ok(7)), Err(Error::Layout));
    m.begin([1; 32]);
    assert_eq!(m.quote(0, 100, || Ok(7)), Ok(7));
    assert_eq!(m.quote(0, 100, || panic!("cache miss")), Ok(7));
    assert_eq!(
        m.quote(1, 100, || Err(Error::Capacity)),
        Err(Error::Capacity)
    );
    assert_eq!(
        m.quote(1, 100, || panic!("failure cache miss")),
        Err(Error::Capacity)
    );
    assert_eq!((m.hits, m.evaluations), (2, 2));
    m.begin([2; 32]);
    assert_eq!(m.quote(0, 100, || Ok(9)), Ok(9));
    assert_eq!(m.quote(1, 100, || Ok(8)), Ok(8));
    assert_eq!((m.hits, m.evaluations), (0, 2));
}
#[test]
fn saturation_is_bounded_and_never_changes_quote_truth() {
    let mut m = QuoteMemo::default();
    m.begin([9; 32]);
    for input in 1..20000 {
        for edge in 0..8 {
            assert_eq!(
                m.quote(edge, input, || Ok(input + edge as u64)),
                Ok(input + edge as u64)
            );
        }
    }
    assert!(m.probe_fallbacks > 0);
    m.begin([10; 32]);
    assert_eq!(m.quote(0, 1, || Ok(88)), Ok(88));
    assert_eq!(m.probe_fallbacks, 0);
}
