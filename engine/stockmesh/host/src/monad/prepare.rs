//! The PR02 boundary validates an already-admitted product; it never invents
//! a venue quote, transaction target, calldata, or wallet signature.
use crate::{monad_contract::{Catalog, Operation}, Result};
use super::orders::{Intent, Order, Side};

pub fn admitted_order(catalog: &Catalog, intent: Intent) -> Result<Order> {
    catalog.validate()?;
    intent.validate()?;
    let operation = match intent.side { Side::Buy => Operation::Buy, Side::Sell => Operation::Sell };
    let product = catalog.products.iter().find(|p| p.asset_id().ok().as_deref() == Some(&intent.asset_id))
        .ok_or("Monad product is not admitted")?;
    if !product.operations.iter().any(|op| op.operation == operation
        && catalog.admitted_venues.iter().any(|venue| venue.venue_id == op.venue_id)) {
        return Err("Monad operation lacks admitted venue".into());
    }
    Order::new(intent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monad::orders::fixture_intent;
    #[test]
    fn observed_token_without_admitted_buy_sell_is_not_preparable() {
        let catalog: Catalog = serde_json::from_str(include_str!("../../../monad/catalog/registry.v1.json")).unwrap();
        assert_eq!(catalog.token_observations.len(), 112);
        assert!(catalog.products.is_empty());
        assert!(admitted_order(&catalog, fixture_intent("mon_unadmitted", "idempotency_key_0003")).is_err());
    }
}
