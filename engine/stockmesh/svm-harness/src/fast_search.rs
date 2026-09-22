//! Native / sampled candidate ranking with exact SBF verification of finalists.
//! Cold surface construction is accounted separately; no sampled output is settled.
use crate::{matrix::*, native_bridge::Curve};
use mollusk_svm::Mollusk;
use serde_json::{json, Value};
use skew_engine::{
    optimizer::oracle::{refine_bounded, ExactPlan},
    Error,
};
use skew_native::surface::{Knot, Surface};
use solana_instruction::Instruction;
use std::time::Instant;
use stocklana_adapters::graph::Venue;

pub struct Compiled<'a> {
    case: &'a Case,
    runtime: &'a Mollusk,
    legs: &'a [(Venue, Instruction, bool)],
    curves: Vec<Option<Curve>>,
    surfaces: Vec<Surface>,
    pub metrics: Value,
}
impl<'a> Compiled<'a> {
    pub fn build(
        m: &'a Mollusk,
        c: &'a Case,
        legs: &'a [(Venue, Instruction, bool)],
        max_input: u64,
    ) -> Self {
        let start = Instant::now();
        let mut curves = Vec::new();
        let mut surfaces = Vec::new();
        let mut rows = Vec::new();
        let mut exact = 0;
        for (v, ix, dir) in legs {
            let curve =
                Curve::decode(&c.a, *v, ix, *dir, c.slot, c.time, m.sysvars.clock.epoch).ok();
            let mut surface = Surface::new(c.slot);
            let mut q = 1_000_000u64;
            let mut samples = 0;
            let mut last_good = 0;
            let mut first_failed = None;
            while q <= max_input {
                let mut call = ix.clone();
                let mut b = [0u8; 80];
                let n = v.swap_data(q, *dir, &mut b).unwrap();
                call.data = b[..n].to_vec();
                let r = run(m, &call, &c.a);
                exact += 1;
                if r.program_result.is_ok() && START - amount(&r.resulting_accounts, &c.input) == q
                {
                    let out = amount(&r.resulting_accounts, &c.output) - START;
                    if let Some(native) = &curve {
                        if let Ok(predicted) = native.quote(q) {
                            assert_eq!(
                                predicted, out,
                                "native/SBF mismatch during surface publication"
                            );
                        }
                    }
                    if surface
                        .push(Knot {
                            input: q,
                            output: out,
                            cu: r.compute_units_consumed + 20_000,
                        })
                        .is_err()
                    {
                        break;
                    }
                    samples += 1;
                    last_good = q;
                } else {
                    first_failed = Some(q);
                    break;
                }
                if q == max_input {
                    break;
                }
                // Native integer curves determine price between probes. Only the
                // resource proposal needs SBF samples; opaque prices keep dense knots.
                q = if curve.is_some() {
                    q.saturating_mul(4).min(max_input)
                } else {
                    (q + q / 4).min(max_input)
                };
            }
            // A failed geometric probe does not make the previous sample the true
            // executable capacity. Resolve the bracket before allocating large orders.
            let mut capacity_probes = 0;
            if let Some(mut high) = first_failed.filter(|_| last_good > 0) {
                for _ in 0..if curve.is_some() { 20 } else { 12 } {
                    if high - last_good < 2 {
                        break;
                    }
                    let q = last_good + (high - last_good) / 2;
                    let mut call = ix.clone();
                    let mut b = [0u8; 80];
                    let n = v.swap_data(q, *dir, &mut b).unwrap();
                    call.data = b[..n].to_vec();
                    let r = run(m, &call, &c.a);
                    exact += 1;
                    capacity_probes += 1;
                    if r.program_result.is_ok()
                        && START.checked_sub(amount(&r.resulting_accounts, &c.input)) == Some(q)
                    {
                        let out = amount(&r.resulting_accounts, &c.output)
                            .checked_sub(START)
                            .unwrap();
                        if let Some(native) = &curve {
                            if let Ok(predicted) = native.quote(q) {
                                assert_eq!(predicted, out, "native/SBF capacity mismatch");
                            }
                        }
                        if surface
                            .push(Knot {
                                input: q,
                                output: out,
                                cu: r.compute_units_consumed + 20_000,
                            })
                            .is_err()
                        {
                            break;
                        }
                        last_good = q;
                        samples += 1;
                    } else {
                        high = q;
                    }
                }
            }
            // Spend cold compilation work where the opaque surface bends most,
            // rather than forcing every quantity band to use the same density.
            // Native curves already supply exact integer prices; their knots model CU only.
            let mut adaptive_samples = 0;
            if curve.is_none() {
                for _ in 0..64 {
                    let Some(q) = surface.refinement_input() else {
                        break;
                    };
                    let mut call = ix.clone();
                    let mut b = [0u8; 80];
                    let n = v.swap_data(q, *dir, &mut b).unwrap();
                    call.data = b[..n].to_vec();
                    let r = run(m, &call, &c.a);
                    exact += 1;
                    if r.program_result.is_err()
                        || START.checked_sub(amount(&r.resulting_accounts, &c.input)) != Some(q)
                    {
                        break;
                    }
                    let Some(out) = amount(&r.resulting_accounts, &c.output).checked_sub(START)
                    else {
                        break;
                    };
                    if surface
                        .insert_refinement(Knot {
                            input: q,
                            output: out,
                            cu: r.compute_units_consumed + 20_000,
                        })
                        .is_err()
                    {
                        break;
                    }
                    adaptive_samples += 1;
                }
            }
            rows.push(json!({"venue":format!("{v:?}"),"native":curve.is_some(),"samples":samples,"adaptive_samples":adaptive_samples,"capacity_probes":capacity_probes,"observed_capacity":last_good}));
            curves.push(curve);
            surfaces.push(surface);
        }
        Self {
            case: c,
            runtime: m,
            legs,
            curves,
            surfaces,
            metrics: json!({"elapsed_ns":start.elapsed().as_nanos() as u64,"sbf_samples":exact,"edges":rows,"scope":"cold compilation, separate from warm request latency"}),
        }
    }
    pub fn solve(&self, q: u64) -> Result<(ExactPlan, Value), Error> {
        // The compiled proposal owns immutable borrows of its exact bank and
        // instruction set. A different state cannot be substituted by slot number.
        let (c, m, legs) = (self.case, self.runtime, self.legs);
        let start = Instant::now();
        let mut candidates = Vec::new();
        // One warm, in-process optimizer result for each CU envelope. Resource prices
        // alter discrete leg activation; actual SBF is the final cost authority.
        for limit in [1_400_000, 1_350_000, 700_000] {
            let plan = refine_bounded(legs.len(), q, 4096, limit, |i, x| {
                let (estimated, cu) = self.surfaces[i]
                    .estimate(x, c.slot)
                    .map_err(|_| Error::Capacity)?;
                let out = match &self.curves[i] {
                    Some(k) => k.quote(x).map_err(|_| Error::Capacity)?,
                    None => estimated,
                };
                Ok((out, cu))
            });
            if let Ok(p) = plan {
                if !candidates.iter().any(|x: &ExactPlan| x.inputs == p.inputs) {
                    candidates.push(p);
                }
            }
        }
        let planning_ns = start.elapsed().as_nanos() as u64;
        // Cold publication already proves native curves against SBF and records
        // executable capacity. Rank single-venue fallbacks from that immutable
        // surface, then exact-verify the best two through the same graph path as
        // every other finalist. This removes an O(venues) SBF fanout from every
        // warm request while retaining a second fallback if the first rejects.
        let mut singles = Vec::new();
        for i in 0..legs.len() {
            let Ok((estimated, cu)) = self.surfaces[i].estimate(q, c.slot) else {
                continue;
            };
            let out = match &self.curves[i] {
                Some(curve) => {
                    let Ok(out) = curve.quote(q) else {
                        continue;
                    };
                    out
                }
                None => estimated,
            };
            if out > 0 && cu <= 1_350_000 {
                singles.push((out, cu, i));
            }
        }
        singles.sort_unstable_by(|left, right| right.0.cmp(&left.0));
        let mut fallback_candidates = 0;
        for (out, cu, i) in singles.into_iter().take(2) {
            let mut single = ExactPlan::default();
            single.inputs[i] = q;
            single.output = out;
            single.outputs[i] = out;
            single.cost = cu;
            single.costs[i] = cu;
            if !candidates.iter().any(|x| x.inputs == single.inputs) {
                candidates.push(single);
                fallback_candidates += 1;
            }
        }
        let mut best = ExactPlan::default();
        let mut verified = 0;
        let mut rejected = 0;
        let mut errors = Vec::new();
        for mut p in candidates {
            p.output = 0;
            p.cost = 0;
            let mut direct = Vec::new();
            for (i, leg) in legs.iter().enumerate() {
                if p.inputs[i] == 0 {
                    continue;
                }
                let (v, mut ix, d) = leg.clone();
                let mut b = [0; 80];
                let n = v.swap_data(p.inputs[i], d, &mut b).unwrap();
                ix.data = b[..n].to_vec();
                direct.push((v, ix, d));
            }
            // This is an unsigned quote simulation. The selected transaction is rebuilt
            // with its verified output floor, then simulated again by the caller.
            let ix = c.graph_mixed(&direct, q, 1).map_err(|_| Error::Bounds)?;
            let r = run(m, &ix, &c.a);
            verified += 1;
            if r.program_result.is_ok() && START - amount(&r.resulting_accounts, &c.input) == q {
                p.output = amount(&r.resulting_accounts, &c.output) - START;
                p.cost = r.compute_units_consumed;
                let mut leg_index = 0;
                for i in 0..legs.len() {
                    if p.inputs[i] > 0 {
                        p.outputs[i] =
                            stocklana_adapters::u64_at(&r.return_data, 32 + leg_index * 32 + 24)
                                .map_err(|_| Error::Arithmetic)?;
                        leg_index += 1;
                    }
                }
                if p.output > best.output {
                    best = p;
                }
            } else {
                rejected += 1;
                errors.push(format!("{:?}", r.program_result));
            }
        }
        if best.output == 0 {
            return Err(Error::Capacity);
        }
        Ok((
            best,
            json!({"native_planning_ns":planning_ns,"warm_solve_and_verify_ns":start.elapsed().as_nanos() as u64,"exact_finalists":verified,"fallback_single_candidates":fallback_candidates,"rejected_finalists":rejected,"errors":errors,"sampled_curves_are_estimates":true}),
        ))
    }
}
