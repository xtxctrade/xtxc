use skew_native::{Error, ScaledUiAmount};

fn mint(multiplier: f64, effective: i64, next: f64, decimals: u8) -> Vec<u8> {
    let mut mint = vec![0u8; 166 + 4 + 56];
    mint[44] = decimals;
    mint[45] = 1;
    mint[165] = 1;
    mint[166..168].copy_from_slice(&25u16.to_le_bytes());
    mint[168..170].copy_from_slice(&56u16.to_le_bytes());
    mint[202..210].copy_from_slice(&multiplier.to_le_bytes());
    mint[210..218].copy_from_slice(&effective.to_le_bytes());
    mint[218..226].copy_from_slice(&next.to_le_bytes());
    mint
}

#[test]
fn scaled_ui_exposure_is_q32_and_changes_at_the_onchain_timestamp() {
    let bytes = mint(1.25, 200, 1.5, 8);
    let before = ScaledUiAmount::decode(&bytes, 199).unwrap();
    let after = ScaledUiAmount::decode(&bytes, 200).unwrap();
    assert_eq!(before.multiplier_q32, 5u64 << 30);
    assert_eq!(after.multiplier_q32, 3u64 << 31);
    assert_eq!(before.exposure_q32(200_000_000, 1, 1, 10_000).unwrap(), 5u64 << 31);
    assert_eq!(after.exposure_q32(200_000_000, 1, 1, 10_000).unwrap(), 3u64 << 32);
}

#[test]
fn conversion_rounds_down_and_rejects_unsafe_multiplier_encodings() {
    let decoded = ScaledUiAmount::decode(&mint(1.0, i64::MAX, 1.0, 8), 0).unwrap();
    assert_eq!(decoded.exposure_q32(1, 1, 1, 9_999).unwrap(), 42);
    assert_eq!(ScaledUiAmount::decode(&mint(f64::NAN, 0, 1.0, 8), -1), Err(Error::Unsupported));
    assert_eq!(ScaledUiAmount::decode(&mint(-1.0, 0, 1.0, 8), -1), Err(Error::Unsupported));
    let mut absent = mint(1.0, 0, 1.0, 8);
    absent[166..168].copy_from_slice(&18u16.to_le_bytes());
    assert_eq!(ScaledUiAmount::decode(&absent, 0), Err(Error::Unsupported));
}
