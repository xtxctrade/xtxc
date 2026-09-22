use ed25519_dalek::{Signer, SigningKey};
use skew_execution_host::stock::*;
fn context() -> (StockContext, EconomicIntent) {
    let signer = SigningKey::from_bytes(&[17; 32]);
    let mint = bs58::encode([1; 32]).into_string();
    let quote = bs58::encode([2; 32]).into_string();
    let policy = Policy {
        instrument: "NVDA".into(),
        version: 3,
        attestor: bs58::encode(signer.verifying_key().as_bytes()).into_string(),
        quote_mint: quote.clone(),
        products: vec![Product {
            mint: mint.clone(),
            issuer: "fixture-issuer".into(),
            primary_is_synchronous: false,
        }],
        max_state_lag_slots: 2,
    };
    let state = State {
        instrument: "NVDA".into(),
        version: 3,
        sequence: 8,
        slot: 100,
        expires_slot: 102,
        underlying_open: true,
        products: vec![ProductState {
            mint: mint.clone(),
            secondary: true,
            rfq: true,
            primary: true,
            corporate_action_halt: false,
        }],
    };
    let signature =
        bs58::encode(signer.sign(&state.signing_bytes().unwrap()).to_bytes()).into_string();
    (
        StockContext {
            policy,
            state,
            signature,
        },
        EconomicIntent {
            instrument: "NVDA".into(),
            version: 3,
            input_mint: quote,
            input_atoms: 1_000_000,
            output_mint: mint,
            issuer: "fixture-issuer".into(),
            min_output_atoms: 100,
            deadline_slot: 102,
            allow_underlying_closed: false,
        },
    )
}
fn sign(context: &mut StockContext) {
    context.signature = bs58::encode(
        SigningKey::from_bytes(&[17; 32])
            .sign(&context.state.signing_bytes().unwrap())
            .to_bytes(),
    )
    .into_string();
}
#[test]
fn verified_product_versions_and_action_specific_state_are_mandatory() {
    let (mut c, mut i) = context();
    let first = c.admit(&i, 100, Kind::Secondary).unwrap();
    assert!(
        c.admit(&i, 100, Kind::Primary).is_err(),
        "async primary cannot enter an atomic graph"
    );
    i.issuer = "different-issuer".into();
    assert!(c.admit(&i, 100, Kind::Secondary).is_err());
    i.issuer = "fixture-issuer".into();
    i.version = 4;
    assert!(c.admit(&i, 100, Kind::Secondary).is_err());
    i.version = 3;
    assert!(c.admit(&i, 103, Kind::Secondary).is_err());
    c.state.underlying_open = false;
    assert!(
        c.admit(&i, 100, Kind::Secondary).is_err(),
        "tampered attestation"
    );
    sign(&mut c);
    assert!(c.admit(&i, 100, Kind::Secondary).is_err());
    i.allow_underlying_closed = true;
    assert_ne!(first, c.admit(&i, 100, Kind::Secondary).unwrap());
    c.state.products[0].rfq = false;
    sign(&mut c);
    c.admit(&i, 100, Kind::Secondary).unwrap();
    assert!(c.admit(&i, 100, Kind::Rfq).is_err());
    c.state.products[0].corporate_action_halt = true;
    sign(&mut c);
    assert!(c.admit(&i, 100, Kind::Secondary).is_err());
}
#[test]
fn rfq_atomic_ticket_lots_and_expiry_cannot_be_interpolated_away() {
    let mut t = TicketCapacity {
        minimum_input: 100,
        maximum_input: 1000,
        lot: 100,
        partial_fill: false,
        expires_slot: 100,
    };
    t.check(1000, 100).unwrap();
    assert!(t.check(800, 100).is_err());
    t.partial_fill = true;
    t.check(800, 100).unwrap();
    assert!(t.check(810, 100).is_err());
    assert!(t.check(800, 101).is_err());
    assert!(t.check(1100, 100).is_err());
}
