//! Seeded, controllable scheduling of pending items.

/// xorshift64* PRNG: tiny, deterministic, no dependencies.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        // splitmix64 the seed so small seeds (0, 1, 2...) diverge immediately.
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        Rng(if z == 0 { 0x2545_F491_4F6C_DD1D } else { z })
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform-ish value in `0..n` (`n > 0`).
    pub fn below(&mut self, n: usize) -> usize {
        assert!(n > 0, "below(0)");
        (self.next_u64() % n as u64) as usize
    }
}

/// Picks which pending item is delivered next.
#[derive(Debug, Clone, Default)]
pub enum Scheduler {
    /// Oldest first.
    #[default]
    Fifo,
    /// Newest first.
    Lifo,
    /// Seeded random choice.
    Seeded(Rng),
    /// Replay an explicit list of indices (taken modulo the queue length); falls
    /// back to FIFO when exhausted.
    Scripted(Vec<usize>, usize),
}

impl Scheduler {
    pub fn fifo() -> Self {
        Scheduler::Fifo
    }
    pub fn lifo() -> Self {
        Scheduler::Lifo
    }
    pub fn seeded(seed: u64) -> Self {
        Scheduler::Seeded(Rng::new(seed))
    }
    pub fn scripted(order: Vec<usize>) -> Self {
        Scheduler::Scripted(order, 0)
    }

    /// Index of the next item among `len` pending ones (`len > 0`).
    pub fn next_index(&mut self, len: usize) -> usize {
        assert!(len > 0, "next_index on empty queue");
        match self {
            Scheduler::Fifo => 0,
            Scheduler::Lifo => len - 1,
            Scheduler::Seeded(r) => r.below(len),
            Scheduler::Scripted(order, pos) => {
                let i = order.get(*pos).copied().unwrap_or(0) % len;
                *pos += 1;
                i
            }
        }
    }

    /// Remove and return the next item.
    pub fn take<T>(&mut self, items: &mut Vec<T>) -> Option<T> {
        if items.is_empty() {
            return None;
        }
        let i = self.next_index(items.len());
        Some(items.remove(i))
    }

    /// A full delivery order (permutation of `0..len`) as this scheduler would
    /// produce it by repeated `take`.
    pub fn order(&mut self, len: usize) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..len).collect();
        let mut out = Vec::with_capacity(len);
        while let Some(i) = self.take(&mut idx) {
            out.push(i);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeded_is_reproducible_and_permutes() {
        let a = Scheduler::seeded(7).order(10);
        let b = Scheduler::seeded(7).order(10);
        assert_eq!(a, b);
        let mut s = a.clone();
        s.sort();
        assert_eq!(s, (0..10).collect::<Vec<_>>());
        let c = Scheduler::seeded(8).order(10);
        assert_ne!(a, c);
    }

    #[test]
    fn fifo_lifo_scripted() {
        assert_eq!(Scheduler::fifo().order(3), vec![0, 1, 2]);
        assert_eq!(Scheduler::lifo().order(3), vec![2, 1, 0]);
        let mut v = vec!['a', 'b', 'c'];
        let mut s = Scheduler::scripted(vec![2, 0]);
        assert_eq!(s.take(&mut v), Some('c'));
        assert_eq!(s.take(&mut v), Some('a'));
        assert_eq!(s.take(&mut v), Some('b'));
        assert_eq!(s.take(&mut v), None);
    }
}
