use super::*;
pub(super) fn select(mut rows: Vec<(String, bool)>, offset: usize, expected_revision: Option<&str>) -> Result<(Vec<String>, String, usize, usize)> {
    rows.sort_by(|a,b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut hash = Sha256::new();
    for (id, pending) in &rows { hash.update(id.as_bytes()); hash.update([u8::from(*pending)]); }
    let revision = hex(&hash.finalize());
    if offset > rows.len() || (offset > 0 && expected_revision.is_none()) || expected_revision.is_some_and(|r| r != revision) { return Err("order page changed".into()); }
    let total = rows.len();
    let active = rows.iter().filter(|r| r.1).count();
    Ok((rows.into_iter().skip(offset).take(64).map(|r|r.0).collect(), revision, total, active))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pending_orders_cannot_hide_behind_historical_rows() {
        let mut rows = (0..80).map(|n| (format!("stkp_{n:032x}"), false)).collect::<Vec<_>>();
        rows.push(("stkp_ffffffffffffffffffffffffffffffff".into(), true));
        let (first,revision,total,active) = select(rows.clone(),0,None).unwrap();
        assert_eq!(first[0],"stkp_ffffffffffffffffffffffffffffffff");
        assert_eq!((total,active),(81,1));
        let (next,_,_,_) = select(rows.clone(),64,Some(&revision)).unwrap();
        assert_eq!(first.iter().chain(next.iter()).collect::<BTreeSet<_>>().len(),81);
        rows[80].1 = false;
        assert!(select(rows,64,Some(&revision)).is_err());
        assert!(select(vec![],1,None).is_err());
    }
}
