// Based on Qdrant sparse index (https://github.com/qdrant/qdrant)
// Copyright 2021-2026 Qdrant Team <info@qdrant.tech>
// Licensed under Apache License 2.0
// Modified for rag3db sparse-vector extension

//! Memory pool for reusing score buffers across searches.

use parking_lot::Mutex;

const POOL_KEEP_LIMIT: usize = 16;

type PooledScores = Vec<f32>;

/// Handle to a pooled scores buffer. Returns the buffer to the pool on drop.
#[derive(Debug)]
pub struct PooledScoresHandle<'a> {
    pool: &'a ScoresMemoryPool,
    pub scores: PooledScores,
}

impl<'a> PooledScoresHandle<'a> {
    fn new(pool: &'a ScoresMemoryPool, scores: PooledScores) -> Self {
        Self { pool, scores }
    }
}

impl Drop for PooledScoresHandle<'_> {
    fn drop(&mut self) {
        self.pool.return_back(std::mem::take(&mut self.scores));
    }
}

/// Pool of pre-allocated score buffers to avoid repeated allocations during search.
#[derive(Debug)]
pub struct ScoresMemoryPool {
    pool: Mutex<Vec<PooledScores>>,
}

impl ScoresMemoryPool {
    pub fn new() -> Self {
        Self {
            pool: Mutex::new(Vec::with_capacity(POOL_KEEP_LIMIT)),
        }
    }

    /// Get a buffer from the pool, or create a new empty one.
    pub fn get(&self) -> PooledScoresHandle<'_> {
        match self.pool.lock().pop() {
            None => PooledScoresHandle::new(self, Vec::new()),
            Some(data) => PooledScoresHandle::new(self, data),
        }
    }

    fn return_back(&self, data: PooledScores) {
        let mut pool = self.pool.lock();
        if pool.len() < POOL_KEEP_LIMIT {
            pool.push(data);
        }
    }
}

impl Default for ScoresMemoryPool {
    fn default() -> Self {
        Self::new()
    }
}
