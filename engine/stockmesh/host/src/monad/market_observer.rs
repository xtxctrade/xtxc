//! Canonical submission observer for wallet-owned Monday market orders.
//! Finalized submission proves only that the issuer accepted an order, not
//! that stock or cash was delivered. No browser-provided receipt is trusted.
use super::{feed::Rpc, journal::MonadJournal,
    monday_receipts::{decode_submission_exact, read_finalized_receipt}, orders::Order};
use crate::{monad_contract::Catalog, Result};

pub fn refresh_submission(rpc: &mut impl Rpc, catalog: &Catalog,
    journal: &mut MonadJournal, id: &str, owner: &str) -> Result<Order> {
    let order = journal.get(id, owner).ok_or("Monad order not found")?.clone();
    if order.market_issuer_order_id.is_some() { return Ok(order); }
    let binding = order.market_binding.as_ref().ok_or("not a direct issuer order")?;
    let hash = order.tx_hash.as_deref().ok_or("order has no submitted transaction")?;
    let receipt = read_finalized_receipt(rpc, hash)?;
    let submitted = decode_submission_exact(catalog, &receipt, &binding.request, &binding.call)?;
    journal.observe_market_submission(id, &submitted).cloned()
}
