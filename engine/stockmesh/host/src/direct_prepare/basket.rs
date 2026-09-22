//! Basket preparation reuses the operator's program, policy registry, ALT
//! cohorts, read provider and typed wallet setup. There is no fixture fallback.
use super::*;
use crate::basket_wire::{self,PreparedBasket};

pub struct BasketStockQuote {
    pub instrument:String,pub minimum_exposure_q32:u64,
    pub products:Vec<PrepareProduct>,pub proposals:Vec<NativeSwapProposal>,pub economic_reflow:bool,
    pub minimum_cash_atoms:Option<u64>,
}
/// Trusted solver output, NOT a public API's route/account specification.
pub struct BasketPrepareRequest {
    pub publication:crate::strategy::Published,pub owner:Pubkey,pub total_input_atoms:String,
    pub catalog_revision:String,pub quotes:Vec<BasketStockQuote>,
    pub tranche:Option<crate::investment::Tranche>,
}
pub struct BasketCandidate {
    pub feed:Arc<crate::feed::Feed>,pub snapshot:Arc<Snapshot>,pub prepared:PreparedBasket,
}

impl PrepareRuntime {
    pub(crate) fn basket_discovery(&self,keys:Vec<String>)->Result<(Arc<crate::feed::Feed>,Arc<Snapshot>)>{
        if keys.is_empty()||keys.len()>100{return Err("joint basket discovery resource bound".into());}
        self.rpc.check_genesis()?;
        let response=self.rpc.call("getMultipleAccounts",json!([keys,{"encoding":"base64","commitment":"confirmed"}]))?;
        let feed=Arc::new(crate::feed::Feed::new(keys,Duration::from_secs(20),4*1024*1024)?);
        let snapshot=feed.publish(&response)?;Ok((feed,snapshot))
    }
    /// All market keys must originate in ONE joint discovery Bank. Each stock
    /// plan is rechecked against a SINGLE fresh execution Bank including wallet,
    /// ProductPolicy and frozen ALT accounts. A growing account set is rejected
    /// before I/O, never split into incoherent responses.
    pub fn prepare_basket(&self,market_keys:&[String],discovery:&Snapshot,request:BasketPrepareRequest)->Result<BasketCandidate>{
        request.publication.verify()?;
        let range=if let Some(t)=&request.tranche{
            if t.plan!=crate::investment::Plan::from_publication(&request.publication,request.owner.to_string(),request.total_input_atoms.clone(),t.plan.nonce.clone(),t.plan.max_slippage_bps)?{
                return Err("investment immutable plan changed".into());}Some(t.range()?)
        }else{None};
        let published=&request.publication.document.legs[range.clone().unwrap_or(0..request.publication.document.legs.len())];
        if request.owner==Pubkey::default()||request.quotes.len()!=published.len()
            ||!((if request.tranche.is_some(){1}else{2})..=basket_wire::MAX_ATOMIC_STOCKS).contains(&request.quotes.len()) {
            return Err("basket preparation bounds".into());}
        if self.maximum_heap_frame_bytes<262_144{return Err("basket deployment heap admission too small".into());}
        let mut all_keys=BTreeSet::new();let mut optional=BTreeSet::new();let mut assets=BTreeMap::new();let mut tables=BTreeSet::new();let mut legs=Vec::new();let mut minimums=BTreeMap::new();
        for (quote,published) in request.quotes.iter().zip(published){
            if quote.instrument!=published.instrument||quote.minimum_exposure_q32==0||quote.products.is_empty()||quote.products.len()>4 {
                return Err("basket published composition and quotes differ".into());}
            minimums.insert(quote.instrument.clone(),quote.minimum_exposure_q32);
            let funding=basket_wire::funding_cash(&quote.proposals)?;
            let cash=funding.map_or(USDC,|(mint,_)|mint);
            if funding.is_some()!=quote.minimum_cash_atoms.is_some()
                ||funding.is_some_and(|(_,q)|quote.minimum_cash_atoms.is_none_or(|m|m==0||m>q)){
                return Err("basket preparation funding floor".into());}
            let table=self.lookup_tables.for_instrument(&quote.instrument)?;tables.insert(table);
            let mut products=Vec::new();
            for p in &quote.products{
                let mint=parse_key(&p.identity.mint)?;let token_program=parse_key(&p.identity.token_program)?;
                validate_product_edges(&p.product_id,mint,if funding.is_some(){2}else{1},&quote.proposals)?;
                let deployed=self.product(&p.product_id,cash)?;
                let destination=native_wire::wallet_asset(request.owner,mint,discovery)?;
                if p.identity.instrument!=quote.instrument||destination.token_program!=token_program||p.minimum_output_atoms==0
                    ||!matches!(p.model,0|1)||p.numerator==0||p.denominator==0||!(1..=10000).contains(&p.conservative_bps){return Err("basket deployed product metadata".into());}
                products.push(DirectProduct{product_id:p.product_id.clone(),minimum_output_atoms:p.minimum_output_atoms,product:MeshProduct{
                    policy:deployed.policy,claim:None,destination:destination.token,mint,token_program,model:p.model,conservative_bps:p.conservative_bps,
                    policy_version:deployed.policy_version,numerator:p.numerator,denominator:p.denominator}});
            }
            let planned=if funding.is_some(){plan_funded_execution_bank(self.settlement_program,table,request.owner,USDC,cash,&products,&quote.proposals,discovery)?}
                else{plan_direct_execution_bank(self.settlement_program,table,request.owner,USDC,&products,&quote.proposals,discovery)?};
            all_keys.extend(planned.keys);optional.extend(planned.optional_wallet_accounts);
            for asset in planned.assets{if assets.insert(asset.token,asset).is_some_and(|old|old!=asset){return Err("basket wallet asset alias".into());}}
            legs.push(basket_wire::Leg{products,proposals:quote.proposals.clone(),economic_reflow:quote.economic_reflow,minimum_cash_atoms:quote.minimum_cash_atoms});
        }
        if all_keys.len()>100||tables.len()>4||assets.len()>crate::wallet_wire::MAX_WALLET_ASSETS{return Err("basket execution Bank resource bound".into());}
        // Discovery may contain unselected pools. Only selected execution
        // dependencies belong in the wallet Bank; never invent missing rows.
        let selected_keys=market_keys.iter().filter(|k|all_keys.contains(*k)).cloned().collect::<Vec<_>>();
        let current=discovery.project(&selected_keys)?;
        // Validate budget, target order and CURRENT catalog commitment before a
        // metered request. Actual observed nonce/deadline replace these below.
        basket_wire::Intent::from_publication_range(&request.publication,request.owner,&request.total_input_atoms,&request.catalog_revision,0,discovery.slot.checked_add(1).ok_or("basket discovery slot overflow")?,&minimums,range.clone())?;
        let all_keys=all_keys.into_iter().collect::<Vec<_>>();
        let response=self.rpc.call("getMultipleAccounts",json!([all_keys,{"encoding":"base64","commitment":"confirmed","minContextSlot":discovery.slot}]))?;
        let feed=Arc::new(crate::feed::Feed::new_execution_bank(all_keys,optional,Duration::from_secs(20),4*1024*1024)?);
        let snapshot=feed.publish(&response)?;
        if snapshot.slot<discovery.slot||snapshot.project(&selected_keys)?.hash!=current.hash{return Err("basket execution market changed; rebuild".into());}
        for leg in &legs{for p in &leg.products{let state=account_view(&snapshot,&p.product.policy)?;
            if state.owner!=self.settlement_program||state.executable||state.data.get(8..40)!=Some(self.policy_authority.as_ref()){
                return Err("basket ProductPolicy authority differs from deployment".into());}}}
        let tables=tables.into_iter().map(|t|frozen_lookup_table(&snapshot,t)).collect::<Result<Vec<_>>>()?;
        let assets=assets.into_values().collect::<Vec<_>>();let read=|address:&Pubkey|account_view(&snapshot,address);
        let (setup,sequence)=WalletSetup::plan(request.owner,&assets,None,read)?.with_observed_nonce(self.settlement_program,read)?;
        let deadline=snapshot.slot.checked_add(self.deadline_slots).ok_or("basket deadline overflow")?;
        let mut intent=basket_wire::Intent::from_publication_range(&request.publication,request.owner,&request.total_input_atoms,&request.catalog_revision,sequence,deadline,&minimums,range)?;
        intent.maximum_cu=self.compute_unit_limit.min(intent.maximum_cu);
        let (blockhash,last_height)=self.rpc.latest_blockhash_at(snapshot.slot)?;
        let lowering=if let Some(message)=setup.compile_setup_only(&tables,blockhash,intent.maximum_cu)?{
            simulate_wallet_setup(&feed,&snapshot,&self.rpc,&setup,&message)?
        }else{(*snapshot).clone()};
        let compiled=basket_wire::compile_configured(self.settlement_program,intent,legs,&lowering,&setup,&tables,blockhash,basket_wire::ExecutionSettings{
            maximum_policy_age:self.maximum_policy_age,compute_unit_price_micro_lamports:self.compute_unit_price,allow_underlying_closed:self.allow_underlying_closed})?;
        let mut prepared=PreparedBasket::simulate(&feed,&snapshot,&self.rpc,&compiled,last_height)?;
        if let Some(t)=request.tranche{prepared.bind_investment(t)?;}
        Ok(BasketCandidate{feed,snapshot,prepared})
    }
}
