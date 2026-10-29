use std::collections::VecDeque;

use crate::internal::alias;
use crate::internal::pickle::Builder;
use crate::internal::table;
use crate::internal::utils;
use crate::policies::traits;
use crate::policies::traits::HandleExt;
use crate::policies::traits::PolicyExt;
use crate::policies::traits::SharedExt;

pub use super::common::Handle;
pub use super::common::Shared;

/// Shorthand for `self.entries[index - self.front_offset]`
macro_rules! get_handle {
    (&$slf:expr, $index:expr) => {
        &$slf.entries[$index - $slf.front_offset]
    };
    (&mut $slf:expr, $index:expr) => {
        &mut $slf.entries[$index - $slf.front_offset]
    };
}

/// A view into an occupied entry in [`FIFOPolicy`].
pub struct Occupied<'a> {
    /// The parent storage that owns the hash table.
    policy: &'a mut FIFOPolicy,
    /// The shared configuration
    shared: &'a Shared,
    /// Bucket index of the occupied slot; valid until the table is modified.
    index: usize,
}

impl traits::OccupiedExt for Occupied<'_> {
    type Handle = Handle;
    type Shared = Shared;

    #[inline]
    fn replace(self, new: Self::Handle) -> Self::Handle {
        // The replace operation does not modify memory range here in the normal case,
        // but when the GIL is disabled, concurrent modification can cause a segfault.
        // Therefore, in free-threaded versions we have to change the generation version
        // before manipulating memory.
        #[cfg(Py_GIL_DISABLED)]
        self.shared.generation_version().increment();

        let index = *self
            .policy
            .table
            .get_bucket(self.index)
            .expect("occupied bucket index");
        let item = get_handle!(&mut self.policy, index);

        self.policy.currsize = self
            .policy
            .currsize
            .saturating_sub(item.size())
            .saturating_add(new.size());

        std::mem::replace(item, new)
    }

    #[inline]
    fn remove(self) -> Self::Handle {
        self.shared.generation_version().increment();

        let (mut index, _) = self
            .policy
            .table
            .get_bucket_entry(self.index)
            .unwrap_or_else(|_| unreachable!("occupied bucket index"))
            .remove();
        index -= self.policy.front_offset;

        self.policy
            .decrement_indexes(index + 1, self.policy.entries.len());

        let handle = self.policy.entries.remove(index).unwrap();
        self.policy.currsize = self.policy.currsize.saturating_sub(handle.size());
        handle
    }
}

/// A view into a vacant slot in [`FIFOPolicy`].
pub struct Vacant<'a> {
    /// The parent policy that owns the hash table.
    policy: &'a mut FIFOPolicy,
    /// The shared configuration
    shared: &'a Shared,
}

impl traits::VacantExt for Vacant<'_> {
    type Handle = Handle;
    type Shared = Shared;

    #[inline]
    fn would_exceed(&self, extra_size: usize) -> bool {
        self.policy.currsize.saturating_add(extra_size) > self.shared.maxsize()
    }

    #[inline]
    fn evict(&mut self) -> pyo3::PyResult<()> {
        self.policy.evict(self.shared)?;
        Ok(())
    }

    fn insert(self, handle: Self::Handle) {
        self.shared.generation_version().increment();

        self.policy.currsize = self.policy.currsize.saturating_add(handle.size());

        let policy = &mut *self.policy;
        let (entries, front_offset) = (&policy.entries, policy.front_offset);
        policy
            .table
            .insert_unique(handle.key().hash(), entries.len() + front_offset, |index| {
                entries[*index - front_offset].key().hash()
            });
        self.policy.entries.push_back(handle);
    }
}

pub struct FIFOPolicy {
    /// Maps each key to its logical index into [`FIFOPolicy::entries`], enabling O(1) lookups.
    ///
    /// Stored indices are *logical* (i.e. they do not reset when entries are popped from the
    /// front), so they must be adjusted on read: `entries[table[k] - front_offset]`.
    /// As a result, table values grow monotonically over the lifetime of the cache,
    /// but their *count* stays bounded by the cache capacity — this is not a memory concern.
    table: table::Table<usize>,

    /// Insertion-ordered sequence of cached handles, providing O(1) front removal.
    entries: VecDeque<Handle>,

    /// Running total of all stored handles' sizes, maintained incrementally.
    currsize: usize,

    /// Number of handles ever popped from the front of [`FIFOPolicy::entries`].
    ///
    /// Because [`VecDeque`] indices shift on front-removal, naively keeping
    /// [`FIFOPolicy::table`] consistent would require decrementing every stored
    /// index — an O(n) operation. Instead, this counter is incremented on each
    /// pop and subtracted at read time: `entries[table[k] - front_offset]`,
    /// keeping both the pop and the lookup O(1).
    ///
    /// To prevent `usize` overflow in the subtraction, once `front_offset`
    /// reaches `usize::MAX - isize::MAX`, all indices in `table` are decremented
    /// by the current `front_offset` and the counter is reset to zero. This
    /// rewrite is O(n) but occurs so rarely, at most once per
    /// `usize::MAX - isize::MAX` evictions, that it is effectively free in practice.
    front_offset: usize,
}

impl FIFOPolicy {
    /// Creates a new [`FIFOPolicy`].
    ///
    /// The underlying [`VecDeque`] is pre-allocated to hold at least `capacity` entries
    /// without reallocation.
    pub fn new(capacity: usize) -> Self {
        Self {
            table: table::Table::with_capacity(capacity),
            entries: VecDeque::with_capacity(capacity),
            currsize: 0,
            front_offset: 0,
        }
    }

    #[inline]
    pub fn table(&self) -> &table::Table<usize> {
        &self.table
    }

    #[inline]
    pub fn entries(&self) -> &VecDeque<Handle> {
        &self.entries
    }

    #[inline]
    fn decrement_indexes(&mut self, start: usize, end: usize) {
        const MAX_FRONT_OFFSET: usize = cfg_select! {
            feature = "use-small-offset" => u8::MAX as usize,
            not(feature = "use-small-offset") => usize::MAX - isize::MAX as usize,
        };

        // Fast path: shifting the entire front is a single counter increment.
        // Guard against overflow; the full-normalization path below handles that case.
        if start <= 1 && end == self.entries.len() && self.front_offset < MAX_FRONT_OFFSET {
            self.front_offset += 1;
            return;
        }

        if (end - start) > self.table.capacity() / 2 {
            // Table-scan
            // normalize every index (subtract fo) and decrement those in range [start, end).
            let front_offset = self.front_offset;
            for i in self.table.iter_mut() {
                let vd_idx = *i - front_offset;

                *i = if start <= vd_idx && vd_idx < end {
                    vd_idx - 1 // normalize + decrement
                } else {
                    vd_idx // normalize
                };
            }
        } else {
            // Entries-scan
            // decrement the logical indices for entries in range [start, end).
            let front_offset = self.front_offset;
            let shifted = self.entries.range(start..end);
            for (i, entry) in (start..end).zip(shifted) {
                let result = self
                    .table
                    .find_mut(entry.key().hash(), |x| (*x) - front_offset == i)
                    .expect("decrement_indexes: shifted entry missing from table");
                *result -= 1;
            }

            // normalize every stored index by subtracting `fo`.
            //   - Entries in  [start, end): (vd_idx + fo - 1) - fo  =  vd_idx - 1
            //   - All others:               (vd_idx + fo)     - fo  =  vd_idx
            if front_offset != 0 {
                for i in self.table.iter_mut() {
                    *i -= front_offset;
                }
            }
        }

        // Both branches now store raw VecDeque indices, so the offset is zero.
        self.front_offset = 0;
    }

    #[inline]
    pub fn iter(&self) -> utils::RawVecDequeIter<Handle> {
        let (first, second) = self.entries.as_slices();
        utils::RawVecDequeIter::new(first, second)
    }
}

impl PolicyExt for FIFOPolicy {
    type Shared = Shared;
    type Handle = Handle;

    type Occupied<'a>
        = Occupied<'a>
    where
        Self: 'a;

    type Vacant<'a>
        = Vacant<'a>
    where
        Self: 'a;

    const PICKLE_SIZE: usize = 1;

    #[inline]
    fn current_size(&self) -> usize {
        self.currsize
    }

    #[inline]
    fn get(
        &mut self,
        py: pyo3::Python,
        key: &<Self::Handle as HandleExt>::Key,
        _shared: &Self::Shared,
    ) -> pyo3::PyResult<Option<&Self::Handle>> {
        let eq = |index: &usize| get_handle!(&self, *index).key().py_eq(py, key);
        match table::find(&self.table, key.hash(), eq)? {
            Some(index) => Ok(Some(get_handle!(&self, *index))),
            None => Ok(None),
        }
    }

    fn entry<'a>(
        &'a mut self,
        py: pyo3::Python,
        key: &<Self::Handle as HandleExt>::Key,
        shared: &'a Self::Shared,
    ) -> pyo3::PyResult<traits::PolicyEntry<Self::Occupied<'a>, Self::Vacant<'a>>> {
        let eq = |index: &usize| get_handle!(&self, *index).key().py_eq(py, key);
        match table::find_index(&self.table, key.hash(), eq)? {
            Some(index) => {
                let result = Occupied {
                    policy: self,
                    shared,
                    index,
                };
                Ok(traits::PolicyEntry::Occupied(result))
            }
            None => {
                let result = Vacant {
                    policy: self,
                    shared,
                };
                Ok(traits::PolicyEntry::Vacant(result))
            }
        }
    }

    fn evict(&mut self, shared: &Self::Shared) -> pyo3::PyResult<Self::Handle> {
        let front = self.entries.front();
        if front.is_none() {
            return Err(new_py_error!(PyKeyError, ()));
        }

        let front = unsafe { front.unwrap_unchecked() };

        let front_offset = self.front_offset;
        self.table
            .find_entry(front.key().hash(), |index| *index - front_offset == 0)
            .unwrap_or_else(|_| unreachable!("evict: front entry missing from table"))
            .remove();

        shared.generation_version().increment();

        self.decrement_indexes(1, self.entries.len());
        let front = unsafe { self.entries.pop_front().unwrap_unchecked() };

        self.currsize = self.currsize.saturating_sub(front.size());

        Ok(front)
    }

    #[inline]
    fn shrink_to_fit(&mut self, shared: &Self::Shared) {
        shared.generation_version().increment();

        let (entries, front_offset) = (&self.entries, self.front_offset);
        self.table
            .shrink_to(0, |index| entries[*index - front_offset].key().hash());

        self.entries.shrink_to_fit();
    }

    #[inline]
    fn clear(&mut self, shared: &Self::Shared) {
        if self.entries.is_empty() {
            return;
        }

        shared.generation_version().increment();
        self.table.clear();
        self.entries.clear();
        self.currsize = 0;
        self.front_offset = 0;
    }

    fn py_eq(
        &self,
        py: pyo3::Python,
        shared: &Self::Shared,
        other: &Self,
        other_shared: &Self::Shared,
    ) -> pyo3::PyResult<bool> {
        if shared.maxsize() != other_shared.maxsize() || self.table.len() != other.table.len() {
            return Ok(false);
        }

        let mut error = None;

        let result = {
            self.table.iter().all(|x| {
                let handle = get_handle!(&self, *x);
                let key = handle.key();

                match table::find(&other.table, key.hash(), |i| {
                    key.py_eq(py, get_handle!(&other, *i).key())
                }) {
                    Err(e) => {
                        error = Some(e);
                        false
                    }
                    Ok(None) => false,
                    Ok(Some(i)) => {
                        let v1 = handle.value();
                        let v2 = get_handle!(&other, *i).value();
                        // SAFETY: both are live Python objects owned by the caches.
                        match unsafe { utils::pyobject_equal(py, v1.as_ptr(), v2.as_ptr()) } {
                            Ok(eq) => eq,
                            Err(e) => {
                                error = Some(e);
                                false
                            }
                        }
                    }
                }
            })
        };

        error.map_or(Ok(result), Err)
    }

    fn clone_ref(&mut self, py: pyo3::Python<'_>) -> Self {
        let mut entries = VecDeque::with_capacity(self.entries.len());
        for handle in self.entries.iter() {
            entries.push_back(handle.clone_ref(py));
        }

        Self {
            table: self.table.clone(),
            entries,
            currsize: self.currsize,
            front_offset: self.front_offset,
        }
    }

    fn build_pickle(
        &self,
        tuple: &mut crate::internal::pickle::TupleBuilder<
            '_,
            crate::internal::pickle::PickleBuilder,
        >,
    ) -> pyo3::PyResult<()> {
        let mut list = tuple.begin_list()?;

        for handle in self.entries.iter() {
            let mut tuple = list.begin_tuple(2)?;
            tuple.push(handle.key().as_ref())?;
            tuple.push(handle.value())?;
            tuple.end()?;
        }

        list.end()
    }

    fn from_pickle(
        maxsize: usize,
        getsizeof: Option<alias::PyObject>,
        _global_ttl: Option<std::time::Duration>,
        built: pyo3::Bound<'_, pyo3::types::PyTuple>,
    ) -> pyo3::PyResult<(Self::Shared, Self)> {
        use pyo3::types::PyAnyMethods;
        use pyo3::types::PyListMethods;
        use pyo3::types::PyTupleMethods;

        let list = built.get_item(0)?.cast_into::<pyo3::types::PyList>()?;
        let list_length = list.len();

        if list_length > maxsize {
            return Err(new_py_error!(
                PyValueError,
                "list size is incompatible with maxsize"
            ));
        }

        let shared = Shared::new(maxsize, getsizeof);
        let mut slf = Self::new(list.len());

        for bound in list.iter() {
            let (key, value) = bound.extract::<(alias::PyObject, alias::PyObject)>()?;

            let handle = Handle::new(bound.py(), shared.getsizeof(), key, value)?;

            slf.currsize = slf.currsize.saturating_add(handle.size());

            // Adding `slf.front_offset` is unnecessary here
            let entries = &slf.entries;
            slf.table
                .insert_unique(handle.key().hash(), entries.len(), |index| {
                    entries[*index].key().hash()
                });
            slf.entries.push_back(handle);
        }

        Ok((shared, slf))
    }
}
