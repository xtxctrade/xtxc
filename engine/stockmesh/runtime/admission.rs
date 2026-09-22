//! Bounded single-owner admission lane. Concurrent producers synchronize outside
//! this kernel; no unsafe lock-free queue claims. In-flight read/write footprints
//! reserve resources; they remain reserved until explicit completion, even after
//! a deadline. A timeout is NOT evidence that a submitted transaction disappeared.
use crate::{Error, Key, Result};

pub const QUEUE_CAPACITY: usize = 256;
pub const OWNER_CAPACITY: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    pub owner: Key,
    pub nonce: u64,
    pub read_resources: u64,
    pub write_resources: u64,
    pub deadline: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ticket {
    index: usize,
    generation: u64,
}

#[derive(Clone, Copy)]
struct Entry {
    request: Request,
    generation: u64,
    in_flight: bool,
    completed: bool,
}

pub struct Admission {
    entries: [Option<Entry>; QUEUE_CAPACITY],
    generation: u64,
    cursor: usize,
}

impl Default for Admission {
    fn default() -> Self {
        Self {
            entries: [None; QUEUE_CAPACITY],
            generation: 0,
            cursor: 0,
        }
    }
}

impl Admission {
    /// Completed IDs stay as tombstones through their original deadline.
    pub fn admit(&mut self, request: Request, now: u64) -> Result<Ticket> {
        if request.deadline < now {
            return Err(Error::Expired);
        }
        for entry in &mut self.entries {
            if entry.is_some_and(|e| !e.in_flight && e.request.deadline < now) {
                *entry = None;
            }
        }
        let mut owned = 0;
        for e in self.entries.iter().flatten() {
            if e.request.owner == request.owner {
                if e.request.nonce == request.nonce {
                    return Err(Error::Duplicate);
                }
                owned += usize::from(!e.completed);
            }
        }
        if owned >= OWNER_CAPACITY {
            return Err(Error::OwnerLimit);
        }
        let index = self
            .entries
            .iter()
            .position(Option::is_none)
            .ok_or(Error::QueueFull)?;
        self.generation = self.generation.checked_add(1).ok_or(Error::Arithmetic)?;
        self.entries[index] = Some(Entry {
            request,
            generation: self.generation,
            in_flight: false,
            completed: false,
        });
        Ok(Ticket {
            index,
            generation: self.generation,
        })
    }

    pub fn dispatch(&mut self, now: u64) -> Option<(Ticket, Request)> {
        let mut reads = 0;
        let mut writes = 0;
        for e in self.entries.iter().flatten().filter(|e| e.in_flight) {
            reads |= e.request.read_resources;
            writes |= e.request.write_resources;
        }
        for offset in 0..QUEUE_CAPACITY {
            let i = (self.cursor + offset) % QUEUE_CAPACITY;
            let Some(e) = &mut self.entries[i] else {
                continue;
            };
            if e.in_flight || e.completed || e.request.deadline < now {
                continue;
            }
            if e.request.write_resources & (reads | writes) != 0
                || e.request.read_resources & writes != 0
            {
                continue;
            }
            e.in_flight = true;
            self.cursor = (i + 1) % QUEUE_CAPACITY;
            return Some((
                Ticket {
                    index: i,
                    generation: e.generation,
                },
                e.request,
            ));
        }
        None
    }

    pub fn complete(&mut self, ticket: Ticket) -> Result<()> {
        let e = self
            .entries
            .get_mut(ticket.index)
            .and_then(Option::as_mut)
            .ok_or(Error::InvalidTicket)?;
        if e.generation != ticket.generation || !e.in_flight || e.completed {
            return Err(Error::InvalidTicket);
        }
        e.in_flight = false;
        e.completed = true;
        Ok(())
    }
}
