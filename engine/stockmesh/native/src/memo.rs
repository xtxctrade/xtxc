//! Bounded per-worker quote memo. Namespace must commit to bank bytes, edge
//! order and decoder policy. Never use a slot number alone as a cache identity.
use crate::{Error, Result};
const CAPACITY: usize = 4096;
const PROBES: usize = 16;
#[derive(Clone, Copy)]
struct Entry {
    epoch: u64,
    input: u64,
    edge: u8,
    value: Result<u64>,
}
pub struct QuoteMemo {
    entries: Box<[Entry]>,
    namespace: Option<[u8; 32]>,
    epoch: u64,
    pub hits: u64,
    pub evaluations: u64,
    pub probe_fallbacks: u64,
}
impl Default for QuoteMemo {
    fn default() -> Self {
        Self {
            entries: vec![
                Entry {
                    epoch: 0,
                    input: 0,
                    edge: 0,
                    value: Err(Error::Capacity)
                };
                CAPACITY
            ]
            .into_boxed_slice(),
            namespace: None,
            epoch: 0,
            hits: 0,
            evaluations: 0,
            probe_fallbacks: 0,
        }
    }
}
impl QuoteMemo {
    pub fn begin(&mut self, namespace: [u8; 32]) {
        self.hits = 0;
        self.evaluations = 0;
        self.probe_fallbacks = 0;
        if self.namespace != Some(namespace) {
            self.namespace = Some(namespace);
            self.epoch = match self.epoch.checked_add(1) {
                Some(e) => e,
                None => {
                    for e in &mut self.entries {
                        e.epoch = 0;
                    }
                    1
                }
            };
        }
    }
    pub fn quote(
        &mut self,
        edge: usize,
        input: u64,
        f: impl FnOnce() -> Result<u64>,
    ) -> Result<u64> {
        if self.namespace.is_none() || edge >= 8 {
            return Err(Error::Layout);
        }
        // Wrapping is confined to hash mixing, never economic arithmetic.
        let mut x = input ^ (edge as u64).wrapping_mul(0x9e3779b97f4a7c15);
        x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
        x ^= x >> 31;
        let start = x as usize & (CAPACITY - 1);
        for p in 0..PROBES {
            let e = &mut self.entries[(start + p) & (CAPACITY - 1)];
            if e.epoch != self.epoch {
                self.evaluations += 1;
                let value = f();
                *e = Entry {
                    epoch: self.epoch,
                    input,
                    edge: edge as u8,
                    value,
                };
                return value;
            }
            if e.input == input && e.edge == edge as u8 {
                self.hits += 1;
                return e.value;
            }
        }
        self.probe_fallbacks += 1;
        self.evaluations += 1;
        f()
    }
}
