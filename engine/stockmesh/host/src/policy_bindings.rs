//! Buy policies bind the economic cash root, not an issuer-conversion edge.
//! Reverse policies retain the native first cash leg. Both PDAs can coexist;
//! adding a root policy never reinterprets or replaces an existing policy.
use crate::{world::WorldConfig, Result};
use std::collections::BTreeSet;

const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const WSOL: &str = "So11111111111111111111111111111111111111112";

pub struct PolicyInputs {
    pub buy_cash: String,
    pub sell_cash: String,
}

pub fn policy_inputs(root:&str,product:&str,worlds:&[WorldConfig],products:&BTreeSet<String>)->Result<PolicyInputs> {
    if ![USDC,WSOL].contains(&root) || !(1..=3).contains(&worlds.len()) || !products.contains(product) {
        return Err("policy route identity/bounds".into());
    }
    let mut previous=root;
    let mut seen=BTreeSet::from([root]);
    for world in worlds {
        let first=world.markets.first().ok_or("policy empty world")?;
        if first.input_mint!=previous || first.input_mint==first.output_mint || !seen.insert(first.output_mint.as_str())
            || world.markets.iter().any(|m|m.input_mint!=first.input_mint || m.output_mint!=first.output_mint) {
            return Err("policy route continuity".into());
        }
        previous=&first.output_mint;
    }
    if previous!=product {return Err("policy route final product".into());}
    let first_output=&worlds[0].markets[0].output_mint;
    let (product_start,buy_cash)=if products.contains(first_output) {(0,root)} else {
        if ![USDC,WSOL].contains(&first_output.as_str()) || first_output==root {return Err("policy funding cash".into());}
        (1,first_output.as_str())
    };
    if worlds[product_start..].iter().any(|world|!products.contains(&world.markets[0].output_mint)) {
        return Err("policy unadmitted intermediate product".into());
    }
    Ok(PolicyInputs{buy_cash:buy_cash.into(),sell_cash:worlds.last().unwrap().markets[0].input_mint.clone()})
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn world(input:&str,output:&str)->WorldConfig {
        serde_json::from_value(json!({"markets":[{"venue":"orca_whirlpool","program":"program","pool":"pool","config":"oracle",
            "input_mint":input,"output_mint":output,"tick_arrays":["array"],"array_capacity":6,"clock":"clock"}],"execution_keys":[]})).unwrap()
    }
    #[test]
    fn economic_root_and_native_reverse_pair_remain_distinct() {
        let products=BTreeSet::from(["SPYx".into(),"SPYon".into()]);
        let worlds=vec![world(USDC,"SPYx"),world("SPYx","SPYon")];
        let p=policy_inputs(USDC,"SPYon",&worlds,&products).unwrap();
        assert_eq!((p.buy_cash.as_str(),p.sell_cash.as_str()),(USDC,"SPYx"));
        let funded=vec![world(WSOL,USDC),world(USDC,"SPYx"),world("SPYx","SPYon")];
        let p=policy_inputs(WSOL,"SPYon",&funded,&products).unwrap();
        assert_eq!((p.buy_cash.as_str(),p.sell_cash.as_str()),(USDC,"SPYx"));
        let p=policy_inputs(USDC,"SPYx",&worlds[..1],&products).unwrap();
        assert_eq!(p.buy_cash,p.sell_cash);
    }
    #[test]
    fn policy_inputs_reject_unadmitted_transit_discontinuity_and_cycles() {
        let products=BTreeSet::from(["SPYx".into(),"SPYon".into()]);
        assert!(policy_inputs(USDC,"SPYon",&[world(USDC,"FAKE"),world("FAKE","SPYon")],&products).is_err());
        assert!(policy_inputs(USDC,"SPYon",&[world(USDC,"SPYx"),world(WSOL,"SPYon")],&products).is_err());
        assert!(policy_inputs(USDC,"SPYx",&[world(USDC,"SPYx"),world("SPYx",USDC),world(USDC,"SPYx")],&products).is_err());
        assert!(policy_inputs(USDC,"SPYon",&[world(USDC,"SPYx")],&products).is_err());
    }
}
