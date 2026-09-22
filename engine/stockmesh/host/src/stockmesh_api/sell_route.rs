//! Select a complete bounded reverse route, not the best isolated first hop.
//! Every candidate spends the same held-mint amount. No partial sell, rounding
//! of input, extra pool or relaxed cash floor is introduced by this planner.
use super::*;

#[derive(Clone)]
pub(super) struct SellStage {
    pub input: u64,
    pub output: u64,
    pub legs: Vec<Value>,
}

pub(super) struct SellRoute {
    pub stages: Vec<SellStage>,
    pub oracle_calls: u64,
    pub native_evaluations: u64,
}

pub(super) fn choose(
    stage_count: usize,
    input: u64,
    mut quote: impl FnMut(usize, u64) -> Result<Value>,
) -> Result<SellRoute> {
    if !(1..=3).contains(&stage_count) || input == 0 || input > MAX_ATOMS {
        return Err("sell input bound".into());
    }
    let mut frontier: Vec<Vec<SellStage>> = vec![Vec::new()];
    let (mut oracle_calls, mut native_evaluations) = (0u64, 0u64);
    for stage in 0..stage_count {
        let mut next = Vec::new();
        // Equivalent first-stage amounts reuse one quote, but distinct amounts
        // must survive: a higher intermediate amount can exceed the next pool.
        let mut memo = BTreeMap::<u64, Option<Value>>::new();
        for path in frontier {
            let amount = path.last().map_or(input, |s| s.output);
            if !memo.contains_key(&amount) {
                let result = match quote(stage, amount) {
                    Ok(result) => {
                        oracle_calls = oracle_calls.checked_add(result["oracleCalls"].as_u64().unwrap_or(0)).ok_or("sell oracle calls")?;
                        native_evaluations = native_evaluations.checked_add(result["nativeEvaluations"].as_u64().unwrap_or(0)).ok_or("sell native evaluations")?;
                        (result["budgetExhausted"] != true).then_some(result)
                    }
                    Err(error) if error == "native allocation: Capacity" => None,
                    Err(error) => return Err(error),
                };
                memo.insert(amount, result);
            }
            let Some(proposal) = memo.get(&amount).and_then(Option::as_ref) else { continue };
            let used = path.iter().map(|s| s.legs.len()).sum::<usize>();
            for (output, legs) in proposal_options(proposal) {
                if used + legs.len() + (stage_count - stage - 1) > MAX_LEGS { continue; }
                let mut spent = 0u64;
                let mut produced = 0u64;
                let mut pools = path.iter().flat_map(|s| &s.legs)
                    .map(|l| l["pool"].as_str().unwrap().to_string()).collect::<BTreeSet<_>>();
                for leg in legs {
                    let pool = leg["pool"].as_str().filter(|v| !v.is_empty()).ok_or("sell option pool")?;
                    let from = leg["inputAtoms"].as_u64().filter(|v| *v > 0).ok_or("sell option input")?;
                    let to = leg["outputAtoms"].as_u64().filter(|v| *v > 0).ok_or("sell option output")?;
                    if !pools.insert(pool.to_string()) { return Err("sell option repeated pool".into()); }
                    spent = spent.checked_add(from).ok_or("sell option input overflow")?;
                    produced = produced.checked_add(to).ok_or("sell option output overflow")?;
                }
                if spent != amount || produced != output {
                    return Err("sell option conservation".into());
                }
                let mut candidate = path.clone();
                candidate.push(SellStage { input: amount, output, legs: legs.to_vec() });
                next.push(candidate);
            }
        }
        if next.is_empty() { return Err("sell bounded route unavailable".into()); }
        // One primary plus at most three alternatives per stage: at most
        // 4^3 complete candidates, each still restricted to four total CPIs.
        if next.len() > 64 { return Err("sell candidate bound".into()); }
        frontier = next;
    }
    let stages = frontier.into_iter().max_by(|a,b| {
        a.last().unwrap().output.cmp(&b.last().unwrap().output)
            .then_with(|| b.iter().map(|s| s.legs.len()).sum::<usize>().cmp(&a.iter().map(|s| s.legs.len()).sum::<usize>()))
    }).ok_or("sell bounded route unavailable")?;
    Ok(SellRoute { stages, oracle_calls, native_evaluations })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn option(input:u64, output:u64, count:usize, prefix:&str)->Value {
        let legs=(0..count).map(|n|json!({"pool":format!("{prefix}{n}"),"inputAtoms":input / count as u64 + u64::from(n==0)* (input % count as u64),"outputAtoms":output / count as u64 + u64::from(n==0)*(output % count as u64)})).collect::<Vec<_>>();
        json!({"outputAtoms":output,"legs":legs})
    }
    #[test]
    fn complete_sell_chooses_lower_first_quote_to_fit_four_legs() {
        let route=choose(2,100,|stage,input|{
            if stage==0 {
                let mut value=option(input,120,4,"a");
                value["alternatives"]=json!([option(input,118,1,"b")]); Ok(value)
            } else { assert_eq!(input,118); Ok(option(input,116,3,"c")) }
        }).unwrap();
        assert_eq!(route.stages[0].input,100);
        assert_eq!(route.stages[1].output,116);
        assert_eq!(route.stages.iter().map(|s|s.legs.len()).sum::<usize>(),4);
    }
    #[test]
    fn capacity_does_not_hide_another_complete_sell_or_state_failure() {
        let first=||{let mut v=option(100,120,1,"a"); v["alternatives"]=json!([option(100,110,1,"b")]); v};
        let route=choose(2,100,|stage,input|match (stage,input) {
            (0,_)=>Ok(first()), (_,120)=>Err("native allocation: Capacity".into()),
            _=>Ok(option(input,109,1,"c")),
        }).unwrap();
        assert_eq!(route.stages.last().unwrap().output,109);
        assert!(choose(2,100,|stage,_|if stage==0{Ok(first())}else{Err("bank changed".into())}).is_err());
    }
    #[test]
    fn malformed_or_partial_sell_never_becomes_a_quote() {
        assert!(choose(4,100,|_,_|unreachable!()).is_err());
        assert!(choose(1,100,|_,_|Ok(option(99,120,1,"a"))).is_err());
        let mut wrong=option(100,120,1,"a"); wrong["outputAtoms"]=json!(121);
        assert!(choose(1,100,|_,_|Ok(wrong.clone())).is_err());
        assert!(choose(2,100,|_,input|Ok(option(input,input+1,1,"repeated"))).is_err());
        let mut exhausted=option(100,120,1,"a"); exhausted["budgetExhausted"]=json!(true);
        assert!(choose(1,100,|_,_|Ok(exhausted.clone())).is_err());
    }

    #[test]
    fn three_hop_reverse_keeps_exact_input_and_reaches_wallet_cash() {
        let r=choose(3,100,|stage,input|Ok(option(input,input-1,if stage==1{2}else{1},&format!("hop{stage}-")))).unwrap();
        assert_eq!(r.stages.iter().map(|s|(s.input,s.output)).collect::<Vec<_>>(),vec![(100,99),(99,98),(98,97)]);
        assert_eq!(r.stages.iter().map(|s|s.legs.len()).sum::<usize>(),4);
        assert!(choose(3,100,|stage,input|Ok(option(input,input-1,2,&format!("hop{stage}-")))).is_err());
        assert!(choose(3,100,|stage,input|if stage==2{Err("bank changed".into())}else{Ok(option(input,input-1,1,&format!("hop{stage}-")))}).is_err());
    }
}
