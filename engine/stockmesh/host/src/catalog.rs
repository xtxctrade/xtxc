//! Discovery is not execution admission. A large catalog never expands signer authority.
use crate::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::Read,
    path::Path,
};

pub const MAX_PRODUCTS: usize = 10_000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AssetKind {
    Equity,
    ListedEtf,
    PreIpo,
    Unclassified,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ExecutionClass {
    SolanaNative,
    EvmNative,
    IssuerAsync,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SourceKind {
    LocalAdmission,
    IssuerCatalog,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Product {
    pub product_id: String,
    pub instrument: String,
    pub name: String,
    pub kind: AssetKind,
    pub issuer: String,
    pub chain: String,
    pub address: String,
    pub execution_class: ExecutionClass,
    pub source_kind: SourceKind,
    pub source_url: String,
    pub source_sha256: String,
    pub observed_at: u64,
    pub issuer_tradable: Option<bool>,
    pub fractional: Option<bool>,
    pub rights_hash: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CatalogFile {
    pub schema: String,
    pub products: Vec<Product>,
}

#[derive(Clone, Default)]
pub struct Catalog {
    pub revision: String,
    pub products: Vec<Product>,
}

pub fn valid_instrument(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'.')
}

fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        && value.bytes().any(|c| c != b'0')
}

impl Product {
    pub fn validate(&self) -> Result<()> {
        let url = reqwest::Url::parse(&self.source_url).map_err(|_| "catalog source URL")?;
        if self.product_id.is_empty()
            || self.product_id.len() > 128
            || !self
                .product_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-:.".contains(&c))
            || !valid_instrument(&self.instrument)
            || self.name.is_empty()
            || self.name.len() > 256
            || self.issuer.is_empty()
            || self.issuer.len() > 128
            || self.observed_at == 0
            || url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !digest(&self.source_sha256)
            || self.rights_hash.as_ref().is_some_and(|hash| !digest(hash))
        {
            return Err("catalog product identity/provenance bounds".into());
        }
        if let Some(id) = self.chain.strip_prefix("eip155:") {
            if id
                .parse::<u64>()
                .ok()
                .filter(|id| *id > 0)
                .map(|id| id.to_string())
                .as_deref()
                != Some(id)
                || self.address.len() != 42
                || !self.address.starts_with("0x")
                || !self.address[2..].bytes().all(|c| c.is_ascii_hexdigit())
                || self.address[2..].bytes().all(|c| c == b'0')
                || self.execution_class == ExecutionClass::SolanaNative
            {
                return Err("catalog EVM asset identity".into());
            }
        } else if let Some(id) = self.chain.strip_prefix("solana:") {
            if bs58::decode(id)
                .into_vec()
                .map_or(true, |v| v.is_empty() || v.len() > 32)
                || bs58::decode(&self.address)
                    .into_vec()
                    .map_or(true, |v| v.len() != 32 || v.iter().all(|b| *b == 0))
                || self.execution_class == ExecutionClass::EvmNative
            {
                return Err("catalog Solana asset identity".into());
            }
        } else {
            return Err("catalog chain namespace unsupported".into());
        }
        Ok(())
    }
}

impl Catalog {
    pub fn new(mut products: Vec<Product>) -> Result<Self> {
        if products.len() > MAX_PRODUCTS {
            return Err("catalog product limit".into());
        }
        products.sort_by(|a, b| a.product_id.cmp(&b.product_id));
        let mut ids = BTreeSet::new();
        let mut identities = BTreeMap::new();
        for product in &products {
            product.validate()?;
            if !ids.insert(&product.product_id) {
                return Err("catalog duplicate product ID".into());
            }
            let address = if product.chain.starts_with("eip155:") {
                product.address.to_ascii_lowercase()
            } else {
                product.address.clone()
            };
            if identities
                .insert((product.chain.clone(), address), &product.product_id)
                .is_some()
            {
                return Err("catalog duplicate chain asset".into());
            }
        }
        let bytes = serde_json::to_vec(&products).map_err(|_| "catalog encoding")?;
        let revision = Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Ok(Self { revision, products })
    }

    pub fn read(path: &Path) -> Result<Self> {
        let mut bytes = Vec::new();
        File::open(path)
            .map_err(|_| "catalog file unavailable")?
            .take(16 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "catalog file read")?;
        if bytes.len() > 16 * 1024 * 1024 {
            return Err("catalog file size".into());
        }
        let file: CatalogFile =
            serde_json::from_slice(&bytes).map_err(|_| "catalog file schema")?;
        if file.schema != "skew.stock-catalog/v1" {
            return Err("catalog schema version".into());
        }
        Self::new(file.products)
    }

    /// Locally admitted records cannot be replaced by external catalog imports.
    pub fn merge(self, imported: Self) -> Result<Self> {
        let mut products: BTreeMap<_, _> = self
            .products
            .into_iter()
            .map(|p| (p.product_id.clone(), p))
            .collect();
        for p in imported.products {
            if p.source_kind != SourceKind::IssuerCatalog {
                return Err("external catalog cannot declare local admission".into());
            }
            if let Some(local) = products.values().find(|local| {
                local.source_kind == SourceKind::LocalAdmission
                    && local.chain == p.chain
                    && local.address == p.address
            }) {
                if local.instrument != p.instrument || local.issuer != p.issuer {
                    return Err("issuer catalog conflicts with admitted identity".into());
                }
                // Keep the existing rights-bound ID, never replace it with a list ID.
                continue;
            }
            if let Some(existing) = products.get(&p.product_id) {
                if existing != &p {
                    return Err("catalog import overrides local admission".into());
                }
            } else {
                products.insert(p.product_id.clone(), p);
            }
        }
        Self::new(products.into_values().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn product(index: u64) -> Product {
        Product {
            product_id: format!("issuer:{index}"),
            instrument: format!("T{index}"),
            name: format!("Synthetic {index}"),
            kind: AssetKind::Unclassified,
            issuer: "fixture".into(),
            chain: "eip155:1".into(),
            address: format!("0x{index:040x}"),
            execution_class: ExecutionClass::EvmNative,
            source_kind: SourceKind::IssuerCatalog,
            source_url: "https://example.invalid/catalog.json".into(),
            source_sha256: "a".repeat(64),
            observed_at: 1,
            issuer_tradable: None,
            fractional: None,
            rights_hash: None,
        }
    }
    #[test]
    fn large_fixture_catalog_is_stable_but_has_no_execution_permission() {
        let products: Vec<_> = (1..=5250).map(product).collect();
        let a = Catalog::new(products.clone()).unwrap();
        let b = Catalog::new(products.into_iter().rev().collect()).unwrap();
        assert_eq!(a.revision, b.revision);
        assert_eq!(a.products.len(), 5250);
        assert!(a.products.iter().all(|p| p.rights_hash.is_none()));
    }
    #[test]
    fn chain_asset_duplicates_and_local_admission_spoofing_fail() {
        let p = product(1);
        let mut other = product(2);
        other.address = p.address.clone();
        assert!(Catalog::new(vec![p.clone(), other]).is_err());
        let mut imported = p.clone();
        imported.source_kind = SourceKind::LocalAdmission;
        assert!(Catalog::new(vec![])
            .unwrap()
            .merge(Catalog::new(vec![imported]).unwrap())
            .is_err());
        let mut wrong = p.clone();
        wrong.chain = "eip155:01".into();
        assert!(wrong.validate().is_err());
        let mut wrong = p;
        wrong.source_url = "https://example.invalid/?api-key=private".into();
        assert!(wrong.validate().is_err());
    }
    fn basket() -> BasketRequest {
        BasketRequest {
            catalog_revision: "rev".into(),
            input_atoms: "1000001".into(),
            max_slippage_bps: 20,
            legs: vec![
                BasketLeg {
                    instrument: "NVDA".into(),
                    weight_bps: 5000,
                },
                BasketLeg {
                    instrument: "SPY".into(),
                    weight_bps: 5000,
                },
            ],
        }
    }
    #[test]
    fn basket_integer_cash_conservation_and_permutation() {
        let mut b = basket();
        assert_eq!(b.allocate("rev").unwrap(), vec![500001, 500000]);
        b.legs.reverse();
        assert_eq!(b.allocate("rev").unwrap(), vec![500000, 500001]);
        for total in 2..2000 {
            b.input_atoms = total.to_string();
            assert_eq!(b.allocate("rev").unwrap().iter().sum::<u64>(), total);
        }
    }
    #[test]
    fn basket_stale_duplicate_invalid_weights_and_dust_fail() {
        assert!(basket().allocate("changed").is_err());
        let mut b = basket();
        b.legs[1].instrument = "NVDA".into();
        assert!(b.allocate("rev").is_err());
        let mut b = basket();
        b.legs[1].weight_bps = 4999;
        assert!(b.allocate("rev").is_err());
        let mut b = basket();
        b.input_atoms = "1".into();
        assert!(b.allocate("rev").is_err());
        let mut b = basket();
        b.input_atoms = "010".into();
        assert!(b.allocate("rev").is_err());
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Search {
    #[serde(default)]
    pub query: String,
    pub kind: Option<AssetKind>,
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "default_limit")]
    pub limit: usize,
    pub revision: Option<String>,
}
fn default_limit() -> usize {
    50
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BasketRequest {
    pub catalog_revision: String,
    pub input_atoms: String,
    pub max_slippage_bps: u16,
    pub legs: Vec<BasketLeg>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BasketLeg {
    pub instrument: String,
    pub weight_bps: u16,
}

impl BasketRequest {
    /// Stable largest-remainder allocation conserves every input atom. No float money.
    pub fn allocate(&self, revision: &str) -> Result<Vec<u64>> {
        self.allocate_with_limit(revision,16)
    }
    pub(crate) fn allocate_with_limit(&self, revision: &str, maximum:usize) -> Result<Vec<u64>> {
        let total = self
            .input_atoms
            .parse::<u64>()
            .map_err(|_| "basket input atoms")?;
        if self.catalog_revision != revision
            || self.input_atoms != total.to_string()
            || total == 0
            || total > 10_000_000_000_000_000
            || !(1..=100).contains(&self.max_slippage_bps)
            || self.legs.len() < 2
            || self.legs.len() > maximum
        {
            return Err("basket revision or input bounds".into());
        }
        let mut seen = BTreeSet::new();
        let mut weights = 0u32;
        let mut allocations = Vec::new();
        let mut remainders = Vec::new();
        for (index, leg) in self.legs.iter().enumerate() {
            if !valid_instrument(&leg.instrument)
                || !seen.insert(&leg.instrument)
                || leg.weight_bps == 0
            {
                return Err("basket duplicate or invalid leg".into());
            }
            weights += u32::from(leg.weight_bps);
            let amount = u128::from(total) * u128::from(leg.weight_bps);
            allocations.push((amount / 10_000) as u64);
            remainders.push((amount % 10_000, &leg.instrument, index));
        }
        if weights != 10_000 {
            return Err("basket weights must sum to 10000".into());
        }
        let residual = total - allocations.iter().sum::<u64>();
        remainders.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        for (_, _, index) in remainders.into_iter().take(residual as usize) {
            allocations[index] += 1;
        }
        if allocations.contains(&0) {
            return Err("basket amount below per-leg atom minimum".into());
        }
        Ok(allocations)
    }
}
