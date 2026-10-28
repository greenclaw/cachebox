//! Safe helpers over [`hashbrown::HashTable`] for tables keyed by Python objects.
//!
//! `HashTable` takes an infallible `eq` closure, while comparing Python keys can
//! raise. [`find_index`] stores the first error and makes `eq` return `true`, so
//! probing stops immediately and no further Python code runs with an exception set.

/// Hash table of cache entries.
pub type Table<T> = ::hashbrown::HashTable<T>;

/// Returns the bucket index of the entry for which `eq` returns `Ok(true)`.
///
/// The index stays valid until the table is modified (insert, remove, resize).
#[inline]
pub fn find_index<T>(
    table: &Table<T>,
    hash: u64,
    mut eq: impl FnMut(&T) -> pyo3::PyResult<bool>,
) -> pyo3::PyResult<Option<usize>> {
    let mut error = None;
    let index = table.find_bucket_index(hash, |x| match eq(x) {
        Ok(found) => found,
        Err(e) => {
            error = Some(e);
            true
        }
    });

    match error {
        Some(e) => Err(e),
        None => Ok(index),
    }
}

/// Returns a reference to the entry for which `eq` returns `Ok(true)`.
#[inline]
pub fn find<T>(
    table: &Table<T>,
    hash: u64,
    eq: impl FnMut(&T) -> pyo3::PyResult<bool>,
) -> pyo3::PyResult<Option<&T>> {
    Ok(find_index(table, hash, eq)?.and_then(|i| table.get_bucket(i)))
}

/// Returns a mutable reference to the entry for which `eq` returns `Ok(true)`.
#[inline]
pub fn find_mut<T>(
    table: &mut Table<T>,
    hash: u64,
    mut eq: impl FnMut(&T) -> pyo3::PyResult<bool>,
) -> pyo3::PyResult<Option<&mut T>> {
    let mut error = None;
    let found = table.find_mut(hash, |x| match eq(x) {
        Ok(found) => found,
        Err(e) => {
            error = Some(e);
            true
        }
    });

    match error {
        Some(e) => Err(e),
        None => Ok(found),
    }
}

/// Random bucket probes before [`random_index`] falls back to an O(n) pick.
const RANDOM_PROBES: usize = 32;

/// Returns the bucket index of a uniformly random entry, or `None` if empty.
///
/// Samples random bucket indices and keeps the first occupied one; conditioned on
/// hitting an occupied bucket, every entry is equally likely. Expected O(1) for a
/// full cache; a sparse table falls back to a uniform O(n) pick after
/// [`RANDOM_PROBES`] misses.
pub fn random_index<T>(table: &Table<T>) -> Option<usize> {
    if table.is_empty() {
        return None;
    }

    let buckets = table.num_buckets();
    for _ in 0..RANDOM_PROBES {
        let index = fastrand::usize(0..buckets);
        if table.get_bucket(index).is_some() {
            return Some(index);
        }
    }

    table.iter_buckets().nth(fastrand::usize(0..table.len()))
}

/// A position in a table that survives across Python `__next__` calls.
///
/// Holds no borrow: the caller re-borrows the table on every step and must
/// guarantee (via the generation version) that the table was not modified.
pub struct BucketCursor {
    next: usize,
    remaining: usize,
}

impl BucketCursor {
    #[inline]
    pub fn new<T>(table: &Table<T>) -> Self {
        Self {
            next: 0,
            remaining: table.len(),
        }
    }

    /// Returns the next occupied entry, or `None` when exhausted.
    #[inline]
    pub fn next<'a, T>(&mut self, table: &'a Table<T>) -> Option<&'a T> {
        if self.remaining == 0 {
            return None;
        }

        while self.next < table.num_buckets() {
            let index = self.next;
            self.next += 1;

            if let Some(x) = table.get_bucket(index) {
                self.remaining -= 1;
                return Some(x);
            }
        }

        self.remaining = 0;
        None
    }

    /// Number of entries left to yield.
    #[inline]
    pub fn len(&self) -> usize {
        self.remaining
    }

    /// Returns `true` when there is nothing left to yield.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.remaining == 0
    }
}
