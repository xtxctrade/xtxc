use skew_engine::runtime::admission::*;
use skew_engine::Error;

fn r(owner: u8, nonce: u64, reads: u64, writes: u64) -> Request {
    Request {
        owner: [owner; 32],
        nonce,
        read_resources: reads,
        write_resources: writes,
        deadline: 100,
    }
}

#[test]
fn conflicts_deadlines_dedup_and_aba_are_enforced() {
    let mut lane = Admission::default();
    lane.admit(r(1, 1, 0, 1), 1).unwrap();
    lane.admit(r(2, 1, 1, 0), 1).unwrap();
    lane.admit(r(3, 1, 0, 2), 1).unwrap();
    let (a, _) = lane.dispatch(1).unwrap();
    let (b, independent) = lane.dispatch(1).unwrap();
    assert_eq!(independent.owner, [3; 32]);
    assert!(lane.dispatch(1).is_none());
    // Expiry cannot release a reservation for an in-flight transaction.
    let mut late = r(4, 1, 0, 1);
    late.deadline = 200;
    lane.admit(late, 101).unwrap();
    assert!(lane.dispatch(101).is_none());
    lane.complete(a).unwrap();
    assert_eq!(lane.complete(a), Err(Error::InvalidTicket));
    lane.complete(b).unwrap();
    assert!(lane.dispatch(101).is_some());
}

#[test]
fn bounded_queue_owner_quota_and_completed_tombstones() {
    let mut lane = Admission::default();
    let request = r(1, 1, 0, 0);
    lane.admit(request, 1).unwrap();
    let (ticket, _) = lane.dispatch(1).unwrap();
    lane.complete(ticket).unwrap();
    assert_eq!(lane.admit(request, 1), Err(Error::Duplicate));
    for n in 2..=33 {
        lane.admit(r(1, n, 0, 0), 1).unwrap();
    }
    assert_eq!(lane.admit(r(1, 34, 0, 0), 1), Err(Error::OwnerLimit));
    for n in 33..256 {
        lane.admit(r(2 + (n % 20) as u8, n as u64, 0, 0), 1)
            .unwrap();
    }
    assert_eq!(lane.admit(r(99, 999, 0, 0), 1), Err(Error::QueueFull));
}
