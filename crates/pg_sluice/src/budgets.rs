//! The budget table: each budget's state, in shared memory so that every
//! backend sees the same numbers.
//!
//! Shared memory is a fixed block that the postmaster sets up at startup and
//! every backend maps. Only plain fixed-size data can live in it. A `Vec` or a
//! `HashMap` keeps its contents on the heap of the backend that allocated them,
//! where no other backend can see them, and grows by allocating more, which
//! shared memory can't do. So the table is a fixed array, and a budget finds
//! its slot by hashing its name.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use pgrx::{PGRXSharedMemory, PgAtomic};

/// How many budgets the table holds.
pub const MAX_BUDGETS: usize = sluice_core::rules::DEFAULT_MAX_BUDGETS;

pub static BUDGETS: PgAtomic<BudgetTable> = unsafe { PgAtomic::new(c"sluice_budgets") };

/// `#[repr(C)]` fixes the layout, the way Postgres's own C structs are laid out.
#[repr(C)]
pub struct BudgetTable {
    budgets: [BudgetState; MAX_BUDGETS],
}

/// One budget's state, shared by all backends.
#[repr(C)]
#[derive(Default)]
pub struct BudgetState {
    /// Hash of the budget's name, or 0 while the slot is free.
    name_hash: AtomicU64,
    /// How many of the budget's queries are running right now.
    running: AtomicU32,
}

impl BudgetState {
    #[cfg(any(test, feature = "pg_test"))]
    pub fn running(&self) -> u32 {
        self.running.load(Ordering::Acquire)
    }

    /// Counts one more running query if fewer than `limit` are running.
    /// Otherwise returns how many are.
    pub fn try_start(&self, limit: u32) -> Result<(), u32> {
        // fetch_update retries a compare_exchange until no other backend changed
        // `running` in between. A plain fetch_add and a check afterwards would
        // go over the limit for a moment, and could turn away someone else.
        self.running
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |running| {
                (running < limit).then_some(running + 1)
            })
            .map(|_| ())
    }

    /// Counts one more running query, whatever the limit.
    pub fn start(&self) {
        self.running.fetch_add(1, Ordering::AcqRel);
    }

    pub fn finish(&self) {
        self.running.fetch_sub(1, Ordering::AcqRel);
    }
}

// Safe because every field is an atomic: backends can use them at the same time.
unsafe impl PGRXSharedMemory for BudgetTable {}

impl Default for BudgetTable {
    fn default() -> Self {
        Self {
            budgets: std::array::from_fn(|_| BudgetState::default()),
        }
    }
}

impl BudgetTable {
    pub fn budget(&self, slot: usize) -> &BudgetState {
        &self.budgets[slot]
    }

    /// The slot that holds the state of the budget called `name`. The first
    /// backend to ask claims a free slot; every later one finds the same slot.
    /// None if the table is full.
    pub fn slot_for(&self, name: &str) -> Option<usize> {
        let hash = name_hash(name);
        // Start at the slot the hash points to and walk on from there, so two
        // names that point to the same slot still get one each.
        let start = (hash % MAX_BUDGETS as u64) as usize;
        for i in 0..MAX_BUDGETS {
            let slot = (start + i) % MAX_BUDGETS;
            // Two backends may try to claim the same free slot at once:
            // compare_exchange lets only one of them write its hash.
            let claimed = self.budgets[slot].name_hash.compare_exchange(
                0,
                hash,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            match claimed {
                // The slot was free and is now this budget's.
                Ok(_) => return Some(slot),
                // Already this budget's.
                Err(current) if current == hash => return Some(slot),
                // Another budget's: try the next slot.
                Err(_) => {}
            }
        }
        None
    }
}

/// The same name gets the same hash in every backend. `DefaultHasher::new()`
/// always starts from the same keys, unlike a `HashMap`, which picks random
/// keys in each process.
fn name_hash(name: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    name.hash(&mut hasher);
    // 0 marks a free slot, so no name may hash to it.
    hasher.finish().max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_start_stops_at_the_limit() {
        let budget = BudgetState::default();
        assert_eq!(budget.try_start(2), Ok(()));
        assert_eq!(budget.try_start(2), Ok(()));
        assert_eq!(budget.try_start(2), Err(2));
        budget.finish();
        assert_eq!(budget.try_start(2), Ok(()));
        assert_eq!(budget.running(), 2);
    }

    #[test]
    fn same_name_same_slot() {
        let table = BudgetTable::default();
        let slot = table.slot_for("reports");
        assert!(slot.is_some());
        assert_eq!(table.slot_for("reports"), slot);
    }

    /// Names whose hashes point to the same slot each get one, until the
    /// table is full.
    #[test]
    fn every_name_gets_its_own_slot_until_full() {
        let table = BudgetTable::default();
        let mut slots: Vec<usize> = (0..MAX_BUDGETS)
            .map(|i| table.slot_for(&format!("budget-{i}")).unwrap())
            .collect();
        slots.sort_unstable();
        slots.dedup();
        assert_eq!(slots.len(), MAX_BUDGETS);
        assert_eq!(table.slot_for("one-too-many"), None);
        // Names already in the table still find their slot.
        assert!(table.slot_for("budget-0").is_some());
    }
}
