//! Only a chain observer, not a browser-submitted JSON receipt, may call this
//! reconciliation boundary. PR03 supplies and authenticates that observer.
use crate::Result;
use super::orders::{digest, AttemptKind, Order, Phase};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Observation {
    Included { tx_hash: String, block_hash: String, success: bool },
    Finalized { tx_hash: String, block_hash: String, success: bool },
    Cancelled { tx_hash: String, block_hash: String },
    Reorged { tx_hash: String, former_block_hash: String },
}

pub(crate) fn apply(order: &mut Order, observation: &Observation) -> Result<()> {
    let (tx_hash, block_hash) = match observation {
        Observation::Included { tx_hash, block_hash, .. }
        | Observation::Finalized { tx_hash, block_hash, .. }
        | Observation::Cancelled { tx_hash, block_hash } => (tx_hash, block_hash),
        Observation::Reorged { tx_hash, former_block_hash } => (tx_hash, former_block_hash),
    };
    let Some(attempt) = order.attempts().into_iter().find(|attempt| attempt.hash == *tx_hash) else {
        return Err("receipt does not bind to order transaction".into());
    };
    if !digest(block_hash) {
        return Err("receipt does not bind to order transaction".into());
    }
    match observation {
        Observation::Included { success, .. } if matches!(order.phase, Phase::Submitted | Phase::Unknown | Phase::Included) => {
            if order.included_block_hash.is_some()
                && (order.included_block_hash.as_deref() != Some(block_hash)
                    || order.included_tx_hash.as_deref().unwrap_or(order.tx_hash.as_deref().unwrap_or("")) != tx_hash)
            { return Err("conflicting Monad inclusion".into()); }
            order.included_tx_hash = Some(tx_hash.clone());
            order.included_block_hash = Some(block_hash.clone());
            order.phase = if *success { Phase::Included } else { Phase::Reverted };
        }
        Observation::Finalized { success, .. } if (attempt.kind == AttemptKind::Execution || !*success)
            && matches!(order.phase, Phase::Included | Phase::Reverted)
            && order.included_block_hash.as_deref() == Some(block_hash)
            && order.included_tx_hash.as_deref().unwrap_or(order.tx_hash.as_deref().unwrap_or("")) == tx_hash => {
            order.finalized_block_hash = Some(block_hash.clone());
            order.phase = if *success { Phase::Finalized } else { Phase::Reverted };
        }
        Observation::Cancelled { .. } if attempt.kind == AttemptKind::Cancellation
            && order.phase == Phase::Included
            && order.included_block_hash.as_deref() == Some(block_hash)
            && order.included_tx_hash.as_deref() == Some(tx_hash) => {
            order.finalized_block_hash = Some(block_hash.clone());
            order.phase = Phase::Cancelled;
        }
        Observation::Reorged { .. } if matches!(order.phase, Phase::Included | Phase::Reverted)
            && order.included_block_hash.as_deref() == Some(block_hash)
            && order.included_tx_hash.as_deref().unwrap_or(order.tx_hash.as_deref().unwrap_or("")) == tx_hash
            && order.finalized_block_hash.is_none() => {
            order.included_tx_hash = None;
            order.included_block_hash = None;
            order.phase = Phase::Unknown;
        }
        _ => return Err("invalid Monad receipt transition".into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monad::orders::{fixture_intent, AttemptKind};
    #[test]
    fn reorg_reopens_unknown_then_finalization_requires_matching_block() {
        let mut order = Order::new(fixture_intent("mon_three", "idempotency_key_0004")).unwrap();
        let tx = format!("0x{}", "b".repeat(64));
        let block = format!("0x{}", "c".repeat(64));
        let later = format!("0x{}", "d".repeat(64));
        order.report_submission(&tx, 4).unwrap();
        apply(&mut order, &Observation::Included { tx_hash: tx.clone(), block_hash: block.clone(), success: true }).unwrap();
        assert!(apply(&mut order, &Observation::Finalized { tx_hash: tx.clone(), block_hash: later.clone(), success: true }).is_err());
        apply(&mut order, &Observation::Reorged { tx_hash: tx.clone(), former_block_hash: block }).unwrap();
        assert_eq!(order.phase, Phase::Unknown);
        apply(&mut order, &Observation::Included { tx_hash: tx.clone(), block_hash: later.clone(), success: true }).unwrap();
        apply(&mut order, &Observation::Finalized { tx_hash: tx, block_hash: later, success: true }).unwrap();
        assert_eq!(order.phase, Phase::Finalized);
    }
    #[test]
    fn competing_attempt_cannot_override_inclusion_without_reorg() {
        let mut order = Order::new(fixture_intent("mon_four", "idempotency_key_0005")).unwrap();
        let first = format!("0x{}", "a".repeat(64));
        let second = format!("0x{}", "b".repeat(64));
        let block = format!("0x{}", "c".repeat(64));
        order.report_submission(&first, 4).unwrap();
        order.report_replacement(&second, 4, AttemptKind::Execution).unwrap();
        apply(&mut order, &Observation::Included { tx_hash: first.clone(),
            block_hash: block.clone(), success: true }).unwrap();
        assert!(apply(&mut order, &Observation::Included { tx_hash: second.clone(),
            block_hash: block.clone(), success: true }).is_err());
        apply(&mut order, &Observation::Reorged { tx_hash: first,
            former_block_hash: block.clone() }).unwrap();
        apply(&mut order, &Observation::Included { tx_hash: second.clone(),
            block_hash: block.clone(), success: true }).unwrap();
        apply(&mut order, &Observation::Finalized { tx_hash: second,
            block_hash: block, success: true }).unwrap();
        assert_eq!(order.phase, Phase::Finalized);
    }
}
