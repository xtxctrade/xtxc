use serde_json::json;
use skew_execution_host::feed::Feed;
use std::time::Duration;
#[test]
fn expensive_work_cannot_survive_state_change_or_disconnect_recovery() {
    let f = Feed::new(
        vec![bs58::encode([1; 32]).into_string()],
        Duration::from_secs(1),
        1024,
    )
    .unwrap();
    let response = |slot| json!({"context":{"slot":slot},"value":[{"owner":bs58::encode([2;32]).into_string(),"lamports":1,"executable":false,"data":["AA==","base64"]}]});
    let first = f.publish(&response(10)).unwrap();
    f.validate_fence(&first).unwrap();
    let same = f.publish(&response(10)).unwrap();
    assert!(f.validate_fence(&first).is_err());
    f.invalidate().unwrap();
    assert!(f.validate_fence(&same).is_err());
    let recovered = f.publish(&response(10)).unwrap();
    assert_eq!(first.hash, recovered.hash);
    assert!(f.validate_fence(&first).is_err());
    f.validate_fence(&recovered).unwrap();
    let newer = f.publish(&response(11)).unwrap();
    assert!(f.validate_fence(&recovered).is_err());
    f.validate_fence(&newer).unwrap();
}
