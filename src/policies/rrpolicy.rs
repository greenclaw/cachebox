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

/// A view into an occupied entry in [`RRPolicy`].
pub struct Occupied<'a> {
    /// The parent storage that owns the hash table.
    policy: &'a mut RRPolicy,
    /// The shared configuration
    shared: &'a Shared,
    /// Bucket index of the occupied slot; valid until the table is modified.
    index: usize,
}

impl traits::OccupiedExt for Occupied<'_> {
    type Shared = Shared;
    type Handle = Handle;

    #[inline]
    fn remove(self) -> Self::Handle {
        self.shared.generation_version().increment();

        let (h, _) = self
            .policy
            .table
            .get_bucket_entry(self.index)
            .unwrap_or_else(|_| unreachable!("occupied bucket index"))
            .remove();
        self.policy.currsize = self.policy.currsize.saturating_sub(h.size());
        h
    }

    #[inline]
    fn replace(self, new: Self::Handle) -> Self::Handle {
        // The replace operation does not modify memory range here in the normal case,
        // but when the GIL is disabled, concurrent modification can cause a segfault.
        // Therefore, in free-threaded versions we have to change the generation version
        // before manipulating memory.
        #[cfg(Py_GIL_DISABLED)]
        self.shared.generation_version().increment();

        self.policy.currsize = self.policy.currsize.saturating_add(new.size());
        let slot = self
            .policy
            .table
            .get_bucket_mut(self.index)
            .expect("occupied bucket index");
        let old = std::mem::replace(slot, new);
        self.policy.currsize = self.policy.currsize.saturating_sub(old.size());

        old
    }
}

/// A view into a vacant slot in [`RRPolicy`].
pub struct Vacant<'a> {
    /// The parent policy that owns the hash table.
    policy: &'a mut RRPolicy,
    /// The shared configuration
    shared: &'a Shared,
}

impl traits::VacantExt for Vacant<'_> {
    type Shared = Shared;
    type Handle = Handle;

    #[inline]
    fn would_exceed(&self, extra_size: usize) -> bool {
        self.policy.currsize.saturating_add(extra_size) > self.shared.maxsize()
    }

    #[inline(always)]
    fn evict(&mut self) -> pyo3::PyResult<()> {
        self.policy.evict(self.shared)?;
        Ok(())
    }

    fn insert(self, handle: Self::Handle) {
        self.shared.generation_version().increment();
        self.policy.currsize = self.policy.currsize.saturating_add(handle.size());

        let hash = handle.key().hash();
        self.policy
            .table
            .insert_unique(hash, handle, |x| x.key().hash());
    }
}

pub struct RRPolicy {
    /// The hash table storing all live [`Handle`] entries.
    table: table::Table<Handle>,
    /// Running total of all stored handles' sizes, maintained incrementally.
    currsize: usize,
}

impl RRPolicy {
    /// Creates a new [`RRPolicy`].
    ///
    /// The underlying hash table is pre-allocated to hold at least `capacity` entries
    /// without reallocation.
    pub fn new(capacity: usize) -> Self {
        Self {
            table: table::Table::with_capacity(capacity),
            currsize: 0,
        }
    }

    /// Returns a reference to the underlying hash table.
    #[inline(always)]
    pub fn table(&self) -> &table::Table<Handle> {
        &self.table
    }
}

impl PolicyExt for RRPolicy {
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
        table::find(&self.table, key.hash(), |x| key.py_eq(py, x.key()))
    }

    fn entry<'a>(
        &'a mut self,
        py: pyo3::Python,
        key: &<Self::Handle as HandleExt>::Key,
        shared: &'a Self::Shared,
    ) -> pyo3::PyResult<traits::PolicyEntry<Self::Occupied<'a>, Self::Vacant<'a>>> {
        match table::find_index(&self.table, key.hash(), |x| key.py_eq(py, x.key()))? {
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

    #[inline]
    fn evict(&mut self, shared: &Self::Shared) -> pyo3::PyResult<Self::Handle> {
        let index = table::random_index(&self.table)
            .ok_or_else(|| new_py_error!(PyKeyError, "cache is empty"))?;

        shared.generation_version().increment();

        let (handle, _) = self
            .table
            .get_bucket_entry(index)
            .unwrap_or_else(|_| unreachable!("random_index returns an occupied bucket"))
            .remove();
        self.currsize = self.currsize.saturating_sub(handle.size());
        Ok(handle)
    }

    #[inline]
    fn shrink_to_fit(&mut self, shared: &Self::Shared) {
        shared.generation_version().increment();
        self.table.shrink_to(0, |x| x.key().hash());
    }

    #[inline]
    fn clear(&mut self, shared: &Self::Shared) {
        if self.table.is_empty() {
            return;
        }
        self.table.clear();
        shared.generation_version().increment();
        self.currsize = 0;
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
            self.table.iter().all(|h1| {
                let key = h1.key();

                match table::find(&other.table, key.hash(), |x| key.py_eq(py, x.key())) {
                    Err(e) => {
                        error = Some(e);
                        false
                    }
                    Ok(None) => false,
                    Ok(Some(h2)) => {
                        // SAFETY: both are live Python objects owned by the tables.
                        match unsafe {
                            utils::pyobject_equal(py, h1.value().as_ptr(), h2.value().as_ptr())
                        } {
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
        let mut table = table::Table::with_capacity(self.table.capacity());

        for handle in self.table.iter() {
            table.insert_unique(handle.key().hash(), handle.clone_ref(py), |x| {
                x.key().hash()
            });
        }

        Self {
            table,
            currsize: self.currsize,
        }
    }

    fn build_pickle(
        &self,
        tuple: &mut crate::internal::pickle::TupleBuilder<
            '_,
            crate::internal::pickle::PickleBuilder,
        >,
    ) -> pyo3::PyResult<()> {
        let mut dict = tuple.begin_dict()?;

        for handle in self.table.iter() {
            dict.entry(handle.key().as_ref(), handle.value())?;
        }

        dict.end()
    }

    fn from_pickle(
        maxsize: usize,
        getsizeof: Option<alias::PyObject>,
        _global_ttl: Option<std::time::Duration>,
        built: pyo3::Bound<'_, pyo3::types::PyTuple>,
    ) -> pyo3::PyResult<(Self::Shared, Self)> {
        use pyo3::types::PyDictMethods;
        use pyo3::types::PyTupleMethods;

        let dict = built.get_item(0)?.cast_into::<pyo3::types::PyDict>()?;
        let dict_length = dict.len();

        if dict_length > maxsize {
            return Err(new_py_error!(
                PyValueError,
                "dict size is incompatible with maxsize"
            ));
        }

        let shared = Shared::new(maxsize, getsizeof);
        let mut slf = Self::new(dict.len());

        for (key, value) in dict.iter() {
            let handle = Handle::new(key.py(), shared.getsizeof(), key.unbind(), value.unbind())?;

            slf.currsize = slf.currsize.saturating_add(handle.size());
            let hash = handle.key().hash();
            slf.table.insert_unique(hash, handle, |x| x.key().hash());
        }

        Ok((shared, slf))
    }
}
