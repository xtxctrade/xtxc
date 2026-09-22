use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};
use skew_engine::SCALE;
use skew_execution_host::{
    fair::{self, Engine, Observation, SignedObservation},
    stock::*,
};
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

fn setup() -> (Engine, StockContext, EconomicIntent, Vec<SignedObservation>) {
    let (context, intent) = context();
    let sources = (0..4)
        .map(|i| fair::Source {
            id: i,
            group: if i == 3 { 0 } else { i },
            public_key: bs58::encode(
                SigningKey::from_bytes(&[40 + i as u8; 32])
                    .verifying_key()
                    .as_bytes(),
            )
            .into_string(),
        })
        .collect();
    let policy = fair::Policy {
        instrument: "NVDA".into(),
        issuer: "fixture-issuer".into(),
        base_mint: intent.output_mint.clone(),
        quote_mint: intent.input_mint.clone(),
        version: 3,
        stock_policy_hash: Sha256::digest(serde_json::to_vec(&context.policy).unwrap()).into(),
        sources,
        faulty_groups: 1,
        max_age_slots: 2,
        max_age_ms: 1000,
        max_width_bps: 100,
    };
    let engine = Engine::new(policy).unwrap();
    let xs = (0..4)
        .map(|source_id| {
            resign(Observation {
                source_id,
                policy_hash: engine.policy_hash(),
                stock_state_hash: context.verify(100).unwrap(),
                sequence: 1,
                slot: 100,
                observed_ms: 10000,
                expires_slot: 102,
                expires_ms: 11000,
                low_q32: 2 * SCALE as u64,
                high_q32: 2 * SCALE as u64,
            })
        })
        .collect();
    (engine, context, intent, xs)
}
fn resign(o: Observation) -> SignedObservation {
    let sk = SigningKey::from_bytes(&[40 + o.source_id as u8; 32]);
    let signature = bs58::encode(sk.sign(&o.signing_bytes().unwrap()).to_bytes()).into_string();
    SignedObservation {
        observation: o,
        signature,
    }
}
#[test]
fn authenticated_band_tightens_intent_and_exogenous_crossing() {
    let (mut e, c, mut i, xs) = setup();
    let d = e.evaluate(&c, &xs, 100, 10000).unwrap();
    assert_eq!(
        d.band().groups,
        3,
        "correlated fourth signer has no extra vote"
    );
    assert_eq!(
        d.tighten(&c, &i, 100, 10000).unwrap().min_output_atoms,
        500000
    );
    i.min_output_atoms = 700000;
    assert_eq!(
        d.tighten(&c, &i, 100, 10000).unwrap().min_output_atoms,
        700000
    );
    let cross = d.crossing(&c, 100, 10000, 200, 100, 60, 120).unwrap();
    assert_eq!(
        (cross.quote, cross.base, cross.buyer_quote_residual),
        (120, 60, 80)
    );
    assert!(d.crossing(&c, 100, 10000, 200, 101, 60, 120).is_err());
    assert!(d.validate(&c, 102, 11001).is_err());
    assert!(d.validate(&c, 103, 10900).is_err());
    let mut reverse = xs.clone();
    reverse.reverse();
    assert_eq!(
        e.evaluate(&c, &reverse, 100, 10000).unwrap().commitment(),
        d.commitment()
    );
}
#[test]
fn source_attacks_fail_closed_without_poisoning_watermarks() {
    for attack in [
        "signature",
        "policy",
        "stock_hash",
        "future_slot",
        "future_time",
        "stale_slot",
        "stale_time",
        "expiry_slot",
        "expiry_time",
        "zero",
        "inverted",
        "missing_group",
        "duplicate",
        "two_outliers",
        "disconnected",
        "unknown",
        "overflow_time",
    ] {
        let (mut e, c, _, mut xs) = setup();
        match attack {
            "signature" => xs[0].signature = xs[1].signature.clone(),
            "policy" => xs[0].observation.policy_hash = [0; 32],
            "stock_hash" => xs[0].observation.stock_state_hash = [0; 32],
            "future_slot" => xs[0].observation.slot = 101,
            "future_time" => xs[0].observation.observed_ms = 10001,
            "stale_slot" => xs[0].observation.slot = 97,
            "stale_time" => xs[0].observation.observed_ms = 8999,
            "expiry_slot" => xs[0].observation.expires_slot = 99,
            "expiry_time" => xs[0].observation.expires_ms = 9999,
            "zero" => xs[0].observation.low_q32 = 0,
            "inverted" => xs[0].observation.high_q32 = 1,
            "missing_group" => {
                xs.remove(2);
            }
            "duplicate" => xs[2] = xs[1].clone(),
            "unknown" => xs[0].observation.source_id = 19,
            "overflow_time" => xs[0].observation.observed_ms = u64::MAX,
            "two_outliers" => {
                xs[1].observation.low_q32 = 10 * SCALE as u64;
                xs[1].observation.high_q32 = 10 * SCALE as u64;
                xs[2].observation.low_q32 = 20 * SCALE as u64;
                xs[2].observation.high_q32 = 20 * SCALE as u64;
            }
            "disconnected" => {
                xs[0].observation.high_q32 = 4 * SCALE as u64;
                xs[3].observation.high_q32 = 4 * SCALE as u64;
                xs[2].observation.low_q32 = 4 * SCALE as u64;
                xs[2].observation.high_q32 = 4 * SCALE as u64;
            }
            _ => unreachable!(),
        }
        if attack != "signature" {
            xs = xs.into_iter().map(|s| resign(s.observation)).collect();
        }
        assert!(e.evaluate(&c, &xs, 100, 10000).is_err(), "{attack}");
        let (_, _, _, valid) = setup();
        e.evaluate(&c, &valid, 100, 10000).unwrap();
    }
}
#[test]
fn equivocating_source_replay_and_state_transition_revoke_decisions() {
    let (mut e, mut c, _, mut xs) = setup();
    let d = e.evaluate(&c, &xs, 100, 10000).unwrap();
    xs[0].observation.high_q32 += 1;
    xs[0] = resign(xs[0].observation.clone());
    assert!(e.evaluate(&c, &xs, 100, 10000).is_err());
    xs[0].observation.sequence = 2;
    xs[0] = resign(xs[0].observation.clone());
    e.evaluate(&c, &xs, 100, 10000).unwrap();
    let (_, _, _, old) = setup();
    assert!(e.evaluate(&c, &old, 100, 10000).is_err());
    assert!(e.evaluate(&c, &xs, 99, 9999).is_err());
    c.state.products[0].corporate_action_halt = true;
    sign(&mut c);
    assert!(d.validate(&c, 100, 10000).is_err());
    assert!(e.evaluate(&c, &xs, 100, 10000).is_err());
}
#[test]
fn restored_watermarks_reject_pre_restart_replay_and_policy_substitution() {
    let (mut engine, context, _, mut observations) = setup();
    engine
        .evaluate(&context, &observations, 100, 10000)
        .unwrap();
    for observation in &mut observations {
        observation.observation.sequence = 2;
        *observation = resign(observation.observation.clone());
    }
    engine
        .evaluate(&context, &observations, 100, 10000)
        .unwrap();
    let state = engine.watermarks();
    let (mut restored, restored_context, _, old) = setup();
    restored.restore_watermarks(state.clone()).unwrap();
    assert!(restored
        .evaluate(&restored_context, &old, 100, 10000)
        .is_err());
    let mut substituted = state;
    substituted.policy_hash = [0; 32];
    assert!(restored.restore_watermarks(substituted).is_err());
}
#[test]
fn closed_market_is_explicit_and_never_revives_friday_reference() {
    let (mut e, mut c, mut i, mut xs) = setup();
    c.state.underlying_open = false;
    sign(&mut c);
    for s in &mut xs {
        s.observation.stock_state_hash = c.verify(100).unwrap();
        *s = resign(s.observation.clone());
    }
    let d = e.evaluate(&c, &xs, 100, 10000).unwrap();
    assert!(d.tighten(&c, &i, 100, 10000).is_err());
    i.allow_underlying_closed = true;
    d.tighten(&c, &i, 100, 10000).unwrap();
    assert!(e.evaluate(&c, &xs, 100, 100000).is_err());
}
