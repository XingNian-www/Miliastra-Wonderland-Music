//! 牌局随机数：确定性 SplitMix64，便于用固定种子复现牌局。

use std::time::{SystemTime, UNIX_EPOCH};

pub(super) fn random_seed() -> u64 {
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (time.as_nanos() as u64) ^ (std::process::id() as u64).rotate_left(17)
}

pub(super) struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub(super) fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub(super) fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e3779b97f4a7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
        value ^ (value >> 31)
    }

    pub(super) fn index(&mut self, len: usize) -> usize {
        (self.next() % len.max(1) as u64) as usize
    }

    pub(super) fn shuffle<T>(&mut self, values: &mut [T]) {
        for index in (1..values.len()).rev() {
            values.swap(index, self.index(index + 1));
        }
    }
}
