//! Monad StockMesh catalog boundary. Discovery is not execution authority.
//! This module never constructs calldata or signs a transaction.
use crate::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const MAINNET_CHAIN_ID: u64 = 143;
pub const TESTNET_CHAIN_ID: u64 = 10143;
pub const CATALOG_SCHEMA: &str = "xtxc.monad.stock-catalog/v1";
pub const ETF_COMPOSITION_SCHEMA: &str = "xtxc.monad.etf-composition/v1";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChainPin {
    pub chain_id: u64,
    pub usdc: String,
    pub usdc_decimals: u8,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Evidence {
    pub source_url: String,
    pub observed_at: String,
    pub runtime_code_hash: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Candidate {
    pub instrument_id: String,
    pub issuer: String,
    pub source_url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VenueObservation {
    pub venue_id: String,
    pub contract: String,
    pub role: String,
    pub source_url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VenueAdmission {
    pub venue_id: String,
    pub contract: String,
    pub typed_abi_hash: String,
    pub evidence: Evidence,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Operation {
    Buy,
    Sell,
    Deliver,
    Return,
    VaultDeposit,
    VaultWithdraw,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OperationAdmission {
    pub operation: Operation,
    pub venue_id: String,
    pub evidence: Evidence,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Product {
    pub instrument_id: String,
    pub issuer: String,
    pub issuer_product_id: String,
    pub chain_id: u64,
    pub token_address: String,
    pub token_decimals: u8,
    pub rights_hash: String,
    pub token_evidence: Evidence,
    pub operations: Vec<OperationAdmission>,
}

impl Product {
    /// Same ticker is allowed across issuers; this identity is not a display ticker.
    pub fn asset_id(&self) -> Result<String> {
        instrument(&self.instrument_id)?;
        label(&self.issuer)?;
        label(&self.issuer_product_id)?;
        address(&self.token_address)?;
        Ok(format!(
            "eip155:{}:erc20:{}:{}:{}",
            self.chain_id, self.token_address, self.issuer, self.issuer_product_id
        ))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Catalog {
    pub schema: String,
    pub chain: ChainPin,
    pub candidates: Vec<Candidate>,
    pub venue_observations: Vec<VenueObservation>,
    pub admitted_venues: Vec<VenueAdmission>,
    pub products: Vec<Product>,
}

/// Rational fee terms are supplied by an approved release, never inferred from
/// a ticker, venue response or rounded to whole basis points.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FeeTerms {
    pub release_id: String,
    pub policy_hash: String,
    pub quote_token: String,
    pub numerator: String,
    pub denominator: String,
    pub extra_etf_mint_fee: bool,
    pub extra_etf_redeem_fee: bool,
}

impl FeeTerms {
    pub fn validate(&self, catalog: &Catalog) -> Result<()> {
        catalog.validate()?;
        label(&self.release_id)?;
        hash(&self.policy_hash)?;
        address(&self.quote_token)?;
        let (numerator, denominator) = self.fraction()?;
        if self.quote_token != catalog.chain.usdc
            || denominator == 0
            || numerator > denominator
            || self.extra_etf_mint_fee
            || self.extra_etf_redeem_fee
        {
            return Err("invalid Monad fee terms".into());
        }
        Ok(())
    }

    pub fn fee_atoms(&self, input_atoms: u128) -> Result<u128> {
        let (numerator, denominator) = self.fraction()?;
        if denominator == 0 || numerator > denominator {
            return Err("invalid fee fraction".into());
        }
        let numerator = numerator as u128;
        let denominator = denominator as u128;
        (input_atoms / denominator)
            .checked_mul(numerator)
            .and_then(|whole| {
                whole.checked_add((input_atoms % denominator) * numerator / denominator)
            })
            .ok_or_else(|| "fee arithmetic overflow".to_string())
    }

    fn fraction(&self) -> Result<(u64, u64)> {
        fn parse(value: &str) -> Result<u64> {
            if value.is_empty()
                || value.len() > 20
                || value.len() > 1 && value.starts_with('0')
                || !value.bytes().all(|c| c.is_ascii_digit())
            {
                return Err("invalid fee fraction encoding".into());
            }
            value
                .parse::<u64>()
                .map_err(|_| "fee fraction overflow".into())
        }
        Ok((parse(&self.numerator)?, parse(&self.denominator)?))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EtfConstituent {
    pub asset_id: String,
    pub units_per_share_atoms: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EtfComposition {
    pub schema: String,
    pub version: u32,
    pub share_decimals: u8,
    pub constituents: Vec<EtfConstituent>,
}

impl EtfComposition {
    pub fn validate(&self, catalog: &Catalog) -> Result<()> {
        catalog.validate()?;
        if self.schema != ETF_COMPOSITION_SCHEMA
            || self.version == 0
            || self.share_decimals > 18
            || !(2..=16).contains(&self.constituents.len())
        {
            return Err("invalid Monad ETF composition header".into());
        }
        let mut ids = BTreeSet::new();
        for constituent in &self.constituents {
            let atoms = constituent
                .units_per_share_atoms
                .parse::<u128>()
                .map_err(|_| "invalid ETF constituent quantity")?;
            if atoms == 0
                || constituent.units_per_share_atoms.starts_with('0')
                || !ids.insert(&constituent.asset_id)
            {
                return Err("duplicate or zero ETF constituent".into());
            }
            let product = catalog
                .products
                .iter()
                .find(|product| {
                    product.asset_id().ok().as_deref() == Some(constituent.asset_id.as_str())
                })
                .ok_or("unknown ETF constituent product")?;
            if !catalog.declares_etf_eligible(product) {
                return Err("ETF constituent lacks vault transfer path".into());
            }
        }
        Ok(())
    }
}

impl Catalog {
    pub fn validate(&self) -> Result<()> {
        if self.schema != CATALOG_SCHEMA
            || self.chain.chain_id != MAINNET_CHAIN_ID
            || self.chain.usdc_decimals != 6
        {
            return Err("invalid Monad catalog chain pin".into());
        }
        address(&self.chain.usdc)?;
        let mut candidates = BTreeSet::new();
        for candidate in &self.candidates {
            instrument(&candidate.instrument_id)?;
            label(&candidate.issuer)?;
            source(&candidate.source_url)?;
            if !candidates.insert((&candidate.instrument_id, &candidate.issuer)) {
                return Err("duplicate Monad candidate".into());
            }
        }
        let mut observations = BTreeSet::new();
        for venue in &self.venue_observations {
            label(&venue.venue_id)?;
            label(&venue.role)?;
            address(&venue.contract)?;
            source(&venue.source_url)?;
            if !observations.insert(&venue.venue_id) {
                return Err("duplicate Monad venue observation".into());
            }
        }
        let mut admitted = BTreeSet::new();
        let mut admitted_code = std::collections::BTreeMap::new();
        for venue in &self.admitted_venues {
            label(&venue.venue_id)?;
            address(&venue.contract)?;
            hash(&venue.typed_abi_hash)?;
            evidence(&venue.evidence)?;
            if !admitted.insert(&venue.venue_id) {
                return Err("duplicate admitted Monad venue".into());
            }
            if let Some(observed) = self
                .venue_observations
                .iter()
                .find(|item| item.venue_id == venue.venue_id)
            {
                if observed.contract != venue.contract {
                    return Err("admitted Monad venue differs from observation".into());
                }
            }
            admitted_code.insert(&venue.venue_id, &venue.evidence.runtime_code_hash);
        }
        let mut ids = BTreeSet::new();
        let mut tokens = BTreeSet::new();
        for product in &self.products {
            if product.chain_id != self.chain.chain_id
                || product.token_decimals > 18
                || product.token_address == self.chain.usdc
            {
                return Err("invalid Monad stock token pin".into());
            }
            hash(&product.rights_hash)?;
            evidence(&product.token_evidence)?;
            let id = product.asset_id()?;
            if !ids.insert(id) || !tokens.insert(&product.token_address) {
                return Err("duplicate Monad product identity or token".into());
            }
            let mut operations = BTreeSet::new();
            for op in &product.operations {
                if !admitted.contains(&op.venue_id) || !operations.insert(op.operation) {
                    return Err("unadmitted venue or duplicate Monad operation".into());
                }
                evidence(&op.evidence)?;
                if admitted_code.get(&op.venue_id).map(|hash| hash.as_str())
                    != Some(op.evidence.runtime_code_hash.as_str())
                {
                    return Err("Monad operation venue code hash mismatch".into());
                }
            }
            // An observed ticker or a one-way buy path is not a tradable product.
            if !operations.is_empty()
                && !(operations.contains(&Operation::Buy)
                    && operations.contains(&Operation::Sell)
                    && operations.contains(&Operation::Deliver)
                    && operations.contains(&Operation::Return))
            {
                return Err("incomplete Monad stock lifecycle".into());
            }
            if operations.contains(&Operation::VaultDeposit)
                != operations.contains(&Operation::VaultWithdraw)
            {
                return Err("incomplete Monad vault transfer lifecycle".into());
            }
        }
        Ok(())
    }

    /// Catalog declaration only; an executor must additionally pin a signed release,
    /// live code/ABI, product rights and fresh venue state before preparing an order.
    pub fn declares_tradeable(&self, product: &Product) -> bool {
        self.validate().is_ok() && self.products.contains(product) && {
            let ops: BTreeSet<_> = product.operations.iter().map(|o| o.operation).collect();
            [
                Operation::Buy,
                Operation::Sell,
                Operation::Deliver,
                Operation::Return,
            ]
            .iter()
            .all(|op| ops.contains(op))
        }
    }

    pub fn declares_etf_eligible(&self, product: &Product) -> bool {
        self.declares_tradeable(product) && {
            let ops: BTreeSet<_> = product.operations.iter().map(|o| o.operation).collect();
            ops.contains(&Operation::VaultDeposit) && ops.contains(&Operation::VaultWithdraw)
        }
    }
}

fn address(value: &str) -> Result<()> {
    if value.len() != 42
        || !value.starts_with("0x")
        || !value[2..]
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        || value[2..].bytes().all(|c| c == b'0')
    {
        return Err("invalid lowercase EVM address".into());
    }
    Ok(())
}

fn hash(value: &str) -> Result<()> {
    if value.len() != 66
        || !value.starts_with("0x")
        || !value[2..]
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        || value[2..].bytes().all(|c| c == b'0')
    {
        return Err("invalid nonzero hash".into());
    }
    Ok(())
}

fn label(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 80
        || value.trim() != value
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b' '))
    {
        return Err("invalid Monad catalog label".into());
    }
    Ok(())
}

fn instrument(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 24
        || !value
            .bytes()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'.')
    {
        return Err("invalid instrument id".into());
    }
    Ok(())
}

fn source(value: &str) -> Result<()> {
    if !value.starts_with("https://") || value.len() > 500 || value.contains(char::is_whitespace) {
        return Err("invalid catalog evidence source".into());
    }
    Ok(())
}

fn evidence(value: &Evidence) -> Result<()> {
    source(&value.source_url)?;
    if value.observed_at.len() != 20
        || !value.observed_at.ends_with('Z')
        || !value.observed_at.bytes().enumerate().all(|(i, c)| match i {
            4 | 7 => c == b'-',
            10 => c == b'T',
            13 | 16 => c == b':',
            19 => c == b'Z',
            _ => c.is_ascii_digit(),
        })
    {
        return Err("invalid observation timestamp".into());
    }
    hash(&value.runtime_code_hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn observed_catalog() -> Catalog {
        let catalog: Catalog =
            serde_json::from_str(include_str!("../../monad/catalog/registry.v1.json")).unwrap();
        catalog
    }

    #[test]
    fn registry_is_discovery_only() {
        let catalog = observed_catalog();
        catalog.validate().unwrap();
        assert!(!catalog.candidates.is_empty());
        assert!(catalog.products.is_empty());
        assert!(catalog.admitted_venues.is_empty());
        assert_eq!(
            serde_json::from_str::<Catalog>(&serde_json::to_string(&catalog).unwrap()).unwrap(),
            catalog
        );
    }

    #[test]
    fn disabled_deployment_manifest_pins_exact_catalog_bytes() {
        let bytes = include_bytes!("../../monad/catalog/registry.v1.json");
        let deployment: serde_json::Value = serde_json::from_str(include_str!(
            "../../monad/catalog/deployment-manifest.v1.json"
        ))
        .unwrap();
        assert_eq!(deployment["schema"], "xtxc.monad.deployment/v1");
        assert_eq!(deployment["chainId"], MAINNET_CHAIN_ID);
        assert_eq!(deployment["state"], "DISABLED");
        assert_eq!(
            deployment["catalogSha256"],
            format!("{:x}", Sha256::digest(bytes))
        );
        assert!(deployment["executor"].is_null());
        assert!(deployment["etfFactory"].is_null());
        assert!(deployment["approvedFeePolicy"].is_null());
        assert_eq!(deployment["admittedProducts"].as_array().unwrap().len(), 0);
        assert_eq!(deployment["admittedVenues"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn wrong_chain_address_decimals_fail_closed() {
        let catalog = observed_catalog();
        let mut wrong = catalog.clone();
        wrong.chain.chain_id = TESTNET_CHAIN_ID;
        assert!(wrong.validate().is_err());
        let mut wrong = catalog.clone();
        wrong.chain.usdc = "0x0000000000000000000000000000000000000000".into();
        assert!(wrong.validate().is_err());
        let mut wrong = catalog.clone();
        wrong.chain.usdc_decimals = 18;
        assert!(wrong.validate().is_err());
        let mut wrong = catalog.clone();
        wrong.venue_observations[0].contract = "0xINVALID".into();
        assert!(wrong.validate().is_err());
    }

    #[test]
    fn different_issuer_same_ticker_has_different_asset_id() {
        let mut a = Product {
            instrument_id: "NVDA".into(),
            issuer: "IssuerA".into(),
            issuer_product_id: "NVDA-A".into(),
            chain_id: MAINNET_CHAIN_ID,
            token_address: "0x1111111111111111111111111111111111111111".into(),
            token_decimals: 18,
            rights_hash: format!("0x{}", "a".repeat(64)),
            token_evidence: Evidence {
                source_url: "https://example.com".into(),
                observed_at: "2026-09-23T00:00:00Z".into(),
                runtime_code_hash: format!("0x{}", "b".repeat(64)),
            },
            operations: vec![],
        };
        let first = a.asset_id().unwrap();
        a.issuer = "IssuerB".into();
        assert_ne!(first, a.asset_id().unwrap());
    }

    #[test]
    fn shared_synthetic_vectors_cover_lifecycle_and_spoofing() {
        // Fixture data is never admissible in a live release.
        let catalog: Catalog = serde_json::from_str(include_str!(
            "../../monad/catalog/fixtures/schema-vectors.synthetic.json"
        ))
        .unwrap();
        catalog.validate().unwrap();
        assert_ne!(
            catalog.products[0].asset_id().unwrap(),
            catalog.products[1].asset_id().unwrap()
        );
        assert!(catalog.declares_tradeable(&catalog.products[0]));
        assert!(catalog.declares_etf_eligible(&catalog.products[0]));
        assert!(!catalog.declares_tradeable(&catalog.products[1]));
        let mut missing_sell = catalog.clone();
        missing_sell.products[0]
            .operations
            .retain(|op| op.operation != Operation::Sell);
        assert!(missing_sell.validate().is_err());
        let mut wrong_decimals = catalog.clone();
        wrong_decimals.products[0].token_decimals = 19;
        assert!(wrong_decimals.validate().is_err());
        let mut wrong_venue = catalog.clone();
        wrong_venue.products[0].operations[0].venue_id = "other".into();
        assert!(wrong_venue.validate().is_err());
        let mut duplicate_token = catalog.clone();
        duplicate_token.products[1].token_address =
            duplicate_token.products[0].token_address.clone();
        assert!(duplicate_token.validate().is_err());
        let mut wrong_code = catalog.clone();
        wrong_code.products[0].operations[0]
            .evidence
            .runtime_code_hash = format!("0x{}", "a".repeat(64));
        assert!(wrong_code.validate().is_err());
        let fee = FeeTerms {
            release_id: "fixture-release".into(),
            policy_hash: format!("0x{}", "a".repeat(64)),
            quote_token: catalog.chain.usdc.clone(),
            numerator: "5".into(),
            denominator: "100000".into(),
            extra_etf_mint_fee: false,
            extra_etf_redeem_fee: false,
        };
        fee.validate(&catalog).unwrap();
        assert_eq!(fee.fee_atoms(100_000_000).unwrap(), 5_000);
        let composition = EtfComposition {
            schema: ETF_COMPOSITION_SCHEMA.into(),
            version: 1,
            share_decimals: 18,
            constituents: catalog
                .products
                .iter()
                .map(|product| EtfConstituent {
                    asset_id: product.asset_id().unwrap(),
                    units_per_share_atoms: "1".into(),
                })
                .collect(),
        };
        assert!(composition.validate(&catalog).is_err()); // second token has no vault path
        let mut fully_admitted = catalog.clone();
        fully_admitted.products[1].operations = fully_admitted.products[0].operations.clone();
        fully_admitted.validate().unwrap();
        composition.validate(&fully_admitted).unwrap();
    }
}
