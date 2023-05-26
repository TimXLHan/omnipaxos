use super::ballot_leader_election::Ballot;
use crate::{
    util::{ConfigurationId, IndexEntry, LogEntry, NodeId, SnapshottedEntry},
    CompactionErr,
};
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use std::{
    error::Error,
    fmt::Debug,
    marker::PhantomData,
    ops::{Bound, RangeBounds},
};

/// Type of the entries stored in the log.
pub trait Entry: Clone + Debug {
    #[cfg(not(feature = "serde"))]
    /// The snapshot type for this entry type.
    type Snapshot: Snapshot<Self>;

    #[cfg(feature = "serde")]
    /// The snapshot type for this entry type.
    type Snapshot: Snapshot<Self> + Serialize + for<'a> Deserialize<'a>;
}

/// A StopSign entry that marks the end of a configuration. Used for reconfiguration.
#[derive(Clone, Debug)]
#[allow(missing_docs)]
pub struct StopSignEntry {
    pub stopsign: StopSign,
    pub decided: bool,
}

impl StopSignEntry {
    /// Creates a [`StopSign`].
    pub fn with(stopsign: StopSign, decided: bool) -> Self {
        StopSignEntry { stopsign, decided }
    }
}

/// A StopSign entry that marks the end of a configuration. Used for reconfiguration.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct StopSign {
    /// The identifier for the new configuration.
    pub config_id: ConfigurationId,
    /// The process ids of the new configuration.
    pub nodes: Vec<NodeId>,
    /// Metadata for the reconfiguration. Can be used for pre-electing leader for the new configuration and skip prepare phase when starting the new configuration with the given leader.
    pub metadata: Option<Vec<u8>>,
}

impl StopSign {
    /// Creates a [`StopSign`].
    pub fn with(config_id: ConfigurationId, nodes: Vec<NodeId>, metadata: Option<Vec<u8>>) -> Self {
        StopSign {
            config_id,
            nodes,
            metadata,
        }
    }
}

impl PartialEq for StopSign {
    fn eq(&self, other: &Self) -> bool {
        self.config_id == other.config_id && self.nodes == other.nodes
    }
}

/// Snapshot type. A `Complete` snapshot contains all snapshotted data while `Delta` has snapshotted changes since an earlier snapshot.
#[allow(missing_docs)]
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum SnapshotType<T>
where
    T: Entry,
{
    Complete(T::Snapshot),
    Delta(T::Snapshot),
}

/// Trait for implementing snapshot operations for log entries of type `T` in OmniPaxos.
pub trait Snapshot<T>: Clone + Debug
where
    T: Entry,
{
    /// Create a snapshot from the log `entries`.
    fn create(entries: &[T]) -> Self;

    /// Merge another snapshot `delta` into self.
    fn merge(&mut self, delta: Self);

    /// Whether `T` is snapshottable. If not, simply return `false` and leave the other functions `unimplemented!()`.
    fn use_snapshots() -> bool;

    //fn size_hint() -> u64;  // TODO: To let the system know trade-off of using entries vs snapshot?
}

/// The Result type returned by the storage API.
pub type StorageResult<T> = std::result::Result<T, Box<dyn Error>>;

/// Trait for implementing the storage backend of Sequence Paxos.
pub trait Storage<T>
where
    T: Entry,
{
    /// Appends an entry to the end of the log and returns the log length.
    fn append_entry(&mut self, entry: T) -> StorageResult<u64>;

    /// Appends the entries of `entries` to the end of the log and returns the log length.
    fn append_entries(&mut self, entries: Vec<T>) -> StorageResult<u64>;

    /// Appends the entries of `entries` to the prefix from index `from_index` in the log and returns the log length.
    fn append_on_prefix(&mut self, from_idx: u64, entries: Vec<T>) -> StorageResult<u64>;

    /// Sets the round that has been promised.
    fn set_promise(&mut self, n_prom: Ballot) -> StorageResult<()>;

    /// Sets the decided index in the log.
    fn set_decided_idx(&mut self, ld: u64) -> StorageResult<()>;

    /// Returns the decided index in the log.
    fn get_decided_idx(&self) -> StorageResult<u64>;

    /// Sets the latest accepted round.
    fn set_accepted_round(&mut self, na: Ballot) -> StorageResult<()>;

    /// Returns the latest round in which entries have been accepted.
    fn get_accepted_round(&self) -> StorageResult<Ballot>;

    /// Returns the entries in the log in the index interval of [from, to).
    /// If entries **do not exist for the complete interval**, an empty Vector should be returned.
    fn get_entries(&self, from: u64, to: u64) -> StorageResult<Vec<T>>;

    /// Returns the current length of the log.
    fn get_log_len(&self) -> StorageResult<u64>;

    /// Returns the suffix of entries in the log from index `from`.
    fn get_suffix(&self, from: u64) -> StorageResult<Vec<T>>;

    /// Returns the round that has been promised.
    fn get_promise(&self) -> StorageResult<Ballot>;

    /// Sets the StopSign used for reconfiguration.
    fn set_stopsign(&mut self, s: StopSignEntry) -> StorageResult<()>;

    /// Returns the stored StopSign.
    fn get_stopsign(&self) -> StorageResult<Option<StopSignEntry>>;

    /// Removes elements up to the given [`idx`] from storage.
    fn trim(&mut self, idx: u64) -> StorageResult<()>;

    /// Sets the compacted (i.e. trimmed or snapshotted) index.
    fn set_compacted_idx(&mut self, idx: u64) -> StorageResult<()>;

    /// Returns the garbage collector index from storage.
    fn get_compacted_idx(&self) -> StorageResult<u64>;

    /// Sets the snapshot.
    fn set_snapshot(&mut self, snapshot: Option<T::Snapshot>) -> StorageResult<()>;

    /// Returns the stored snapshot.
    fn get_snapshot(&self) -> StorageResult<Option<T::Snapshot>>;
}

/// A place holder type for when not using snapshots. You should not use this type, it is only internally when deriving the Entry implementation.
#[derive(Copy, Clone, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct NoSnapshot;

impl<T: Entry> Snapshot<T> for NoSnapshot {
    fn create(_entries: &[T]) -> Self {
        panic!("NoSnapshot should not be created");
    }

    fn merge(&mut self, _delta: Self) {
        panic!("NoSnapshot should not be merged");
    }

    fn use_snapshots() -> bool {
        false
    }
}

/// Used to perform convenient rollbacks of storage operations on internal storage.
/// Represents only values that can and will actually be rolled back from outside internal storage.
pub(crate) enum RollbackValue {
    DecidedIdx(u64),
    AcceptedRound(Ballot),
}

/// Internal representation of storage. Hides all complexities with the compacted index
/// such that Sequence Paxos accesses the log with the uncompacted index.
pub(crate) struct InternalStorage<I, T>
where
    I: Storage<T>,
    T: Entry,
{
    storage: I,
    _t: PhantomData<T>,
}

impl<I, T> InternalStorage<I, T>
where
    I: Storage<T>,
    T: Entry,
{
    pub(crate) fn with(storage: I) -> Self {
        InternalStorage {
            storage,
            _t: Default::default(),
        }
    }

    /// Writes the value.
    pub(crate) fn single_rollback(&mut self, value: RollbackValue) {
        match value {
            RollbackValue::DecidedIdx(idx) => self
                .set_decided_idx(idx)
                .expect("storage error while trying to write decided_idx"),
            RollbackValue::AcceptedRound(b) => self
                .set_accepted_round(b)
                .expect("storage error while trying to write accepted_round"),
        }
    }

    /// Writes the values.
    pub(crate) fn rollback(&mut self, values: Vec<RollbackValue>) {
        for value in values {
            self.single_rollback(value);
        }
    }

    /// This function is useful to handle `StorageResult::Error`.
    /// If `result` is an error, this function tries to write the `values` and then panics with `msg`.
    /// Otherwise it returns.
    pub(crate) fn rollback_if_err<R>(
        &mut self,
        result: &StorageResult<R>,
        values: Vec<RollbackValue>,
        msg: &str,
    ) where
        R: Debug,
    {
        if result.is_err() {
            self.rollback(values);
            panic!("{}: {}", msg, result.as_ref().unwrap_err());
        }
    }

    fn get_entry_type(
        &self,
        idx: u64,
        compacted_idx: u64,
        virtual_log_len: u64,
    ) -> StorageResult<Option<IndexEntry>> {
        if idx < compacted_idx {
            Ok(Some(IndexEntry::Compacted))
        } else if idx < virtual_log_len {
            Ok(Some(IndexEntry::Entry))
        } else if idx == virtual_log_len {
            match self.get_stopsign()? {
                Some(ss) if ss.decided => Ok(Some(IndexEntry::StopSign(ss.stopsign))),
                _ => Ok(None),
            }
        } else {
            Ok(None)
        }
    }

    /// Read entries in the range `r` in the log. Returns `None` if `r` is out of bounds.
    pub(crate) fn read<R>(&self, r: R) -> StorageResult<Option<Vec<LogEntry<T>>>>
    where
        R: RangeBounds<u64>,
    {
        let virtual_log_len = self.get_log_len()?;
        let from_idx = match r.start_bound() {
            Bound::Included(i) => *i,
            Bound::Excluded(e) => *e + 1,
            Bound::Unbounded => 0,
        };
        let to_idx = match r.end_bound() {
            Bound::Included(i) => *i + 1,
            Bound::Excluded(e) => *e,
            Bound::Unbounded => {
                let idx = virtual_log_len;
                match self.get_stopsign()? {
                    Some(ss) if ss.decided => idx + 1,
                    _ => idx,
                }
            }
        };
        let compacted_idx = self.get_compacted_idx()?;
        let to_type = match self.get_entry_type(to_idx - 1, compacted_idx, virtual_log_len)? {
            // use to_idx-1 when getting the entry type as to_idx is exclusive
            Some(IndexEntry::Compacted) => {
                return Ok(Some(vec![self.create_compacted_entry(compacted_idx)?]))
            }
            Some(from_type) => from_type,
            _ => return Ok(None),
        };
        let from_type = match self.get_entry_type(from_idx, compacted_idx, virtual_log_len)? {
            Some(from_type) => from_type,
            _ => return Ok(None),
        };
        let decided_idx = self.get_decided_idx()?;
        match (from_type, to_type) {
            (IndexEntry::Entry, IndexEntry::Entry) => {
                let from_suffix_idx = from_idx - compacted_idx;
                let to_suffix_idx = to_idx - compacted_idx;
                Ok(Some(self.create_read_log_entries_with_real_idx(
                    from_suffix_idx,
                    to_suffix_idx,
                    compacted_idx,
                    decided_idx,
                )?))
            }
            (IndexEntry::Entry, IndexEntry::StopSign(ss)) => {
                let from_suffix_idx = from_idx - compacted_idx;
                let to_suffix_idx = to_idx - compacted_idx - 1;
                let mut entries = self.create_read_log_entries_with_real_idx(
                    from_suffix_idx,
                    to_suffix_idx,
                    compacted_idx,
                    decided_idx,
                )?;
                entries.push(LogEntry::StopSign(ss));
                Ok(Some(entries))
            }
            (IndexEntry::Compacted, IndexEntry::Entry) => {
                let from_suffix_idx = 0;
                let to_suffix_idx = to_idx - compacted_idx;
                let mut entries = Vec::with_capacity((to_suffix_idx + 1) as usize);
                let compacted = self.create_compacted_entry(compacted_idx)?;
                entries.push(compacted);
                let mut e = self.create_read_log_entries_with_real_idx(
                    from_suffix_idx,
                    to_suffix_idx,
                    compacted_idx,
                    decided_idx,
                )?;
                entries.append(&mut e);
                Ok(Some(entries))
            }
            (IndexEntry::Compacted, IndexEntry::StopSign(ss)) => {
                let from_suffix_idx = 0;
                let to_suffix_idx = to_idx - compacted_idx - 1;
                let mut entries = Vec::with_capacity((to_suffix_idx + 1) as usize);
                let compacted = self.create_compacted_entry(compacted_idx)?;
                entries.push(compacted);
                let mut e = self.create_read_log_entries_with_real_idx(
                    from_suffix_idx,
                    to_suffix_idx,
                    compacted_idx,
                    decided_idx,
                )?;
                entries.append(&mut e);
                entries.push(LogEntry::StopSign(ss));
                Ok(Some(entries))
            }
            (IndexEntry::StopSign(ss), IndexEntry::StopSign(_)) => {
                Ok(Some(vec![LogEntry::StopSign(ss)]))
            }
            e => {
                unimplemented!("{}", format!("Unexpected read combination: {:?}", e))
            }
        }
    }

    fn create_read_log_entries_with_real_idx(
        &self,
        from_sfx_idx: u64,
        to_sfx_idx: u64,
        compacted_idx: u64,
        decided_idx: u64,
    ) -> StorageResult<Vec<LogEntry<T>>> {
        let entries = self
            .get_entries_with_real_idx(from_sfx_idx, to_sfx_idx)?
            .into_iter()
            .enumerate()
            .map(|(idx, e)| {
                let log_idx = idx as u64 + compacted_idx;
                if log_idx > decided_idx {
                    LogEntry::Undecided(e)
                } else {
                    LogEntry::Decided(e)
                }
            })
            .collect();
        Ok(entries)
    }

    /// Read all decided entries from `from_idx` in the log. Returns `None` if `from_idx` is out of bounds.
    pub(crate) fn read_decided_suffix(
        &self,
        from_idx: u64,
    ) -> StorageResult<Option<Vec<LogEntry<T>>>> {
        let decided_idx = self.get_decided_idx()?;
        if from_idx < decided_idx {
            self.read(from_idx..decided_idx)
        } else {
            Ok(None)
        }
    }

    fn create_compacted_entry(&self, compacted_idx: u64) -> StorageResult<LogEntry<T>> {
        self.storage.get_snapshot().map(|snap| match snap {
            Some(s) => LogEntry::Snapshotted(SnapshottedEntry::with(compacted_idx, s)),
            None => LogEntry::Trimmed(compacted_idx),
        })
    }

    /*** Writing ***/
    pub(crate) fn append_entry(&mut self, entry: T) -> StorageResult<u64> {
        let compacted_idx = self.storage.get_compacted_idx()?;
        self.storage
            .append_entry(entry)
            .map(|idx| idx + compacted_idx)
    }

    pub(crate) fn append_entries(&mut self, entries: Vec<T>) -> StorageResult<u64> {
        let compacted_idx = self.storage.get_compacted_idx()?;
        self.storage
            .append_entries(entries)
            .map(|idx| idx + compacted_idx)
    }

    pub(crate) fn append_on_decided_prefix(&mut self, entries: Vec<T>) -> StorageResult<u64> {
        let decided_idx = self.storage.get_decided_idx()?;
        let compacted_idx = self.storage.get_compacted_idx()?;
        self.storage
            .append_on_prefix(decided_idx - compacted_idx, entries)
            .map(|idx| idx + compacted_idx)
    }

    pub(crate) fn append_on_prefix(
        &mut self,
        from_idx: u64,
        entries: Vec<T>,
    ) -> StorageResult<u64> {
        let compacted_idx = self.storage.get_compacted_idx()?;
        self.storage
            .append_on_prefix(from_idx - compacted_idx, entries)
            .map(|idx| idx + compacted_idx)
    }

    pub(crate) fn set_promise(&mut self, n_prom: Ballot) -> StorageResult<()> {
        self.storage.set_promise(n_prom)
    }

    pub(crate) fn set_decided_idx(&mut self, ld: u64) -> StorageResult<()> {
        self.storage.set_decided_idx(ld)
    }

    pub(crate) fn get_decided_idx(&self) -> StorageResult<u64> {
        self.storage.get_decided_idx()
    }

    pub(crate) fn set_accepted_round(&mut self, na: Ballot) -> StorageResult<()> {
        self.storage.set_accepted_round(na)
    }

    pub(crate) fn get_accepted_round(&self) -> StorageResult<Ballot> {
        self.storage.get_accepted_round()
    }

    pub(crate) fn get_entries(&self, from: u64, to: u64) -> StorageResult<Vec<T>> {
        let compacted_idx = self.storage.get_compacted_idx()?;
        self.get_entries_with_real_idx(from - compacted_idx.min(from), to - compacted_idx.min(to))
    }

    /// Get entries with real physical log indexes i.e. the index with the compacted offset.
    fn get_entries_with_real_idx(
        &self,
        from_sfx_idx: u64,
        to_sfx_idx: u64,
    ) -> StorageResult<Vec<T>> {
        self.storage.get_entries(from_sfx_idx, to_sfx_idx)
    }

    /// The length of the replicated log, as if log was never compacted.
    pub(crate) fn get_log_len(&self) -> StorageResult<u64> {
        let compacted_idx = self.storage.get_compacted_idx()?;
        let len = self.get_real_log_len()?;
        Ok(compacted_idx + len)
    }

    /// The length of the physical log, which can get smaller with compaction
    fn get_real_log_len(&self) -> StorageResult<u64> {
        self.storage.get_log_len()
    }

    pub(crate) fn get_suffix(&self, from: u64) -> StorageResult<Vec<T>> {
        let compacted_idx = self.storage.get_compacted_idx()?;
        self.storage.get_suffix(from - compacted_idx.min(from))
    }

    pub(crate) fn get_promise(&self) -> StorageResult<Ballot> {
        self.storage.get_promise()
    }

    pub(crate) fn set_stopsign(&mut self, s: StopSignEntry) -> StorageResult<()> {
        self.storage.set_stopsign(s)
    }

    pub(crate) fn get_stopsign(&self) -> StorageResult<Option<StopSignEntry>> {
        self.storage.get_stopsign()
    }

    pub(crate) fn create_snapshot(&mut self, compact_idx: u64) -> StorageResult<T::Snapshot> {
        let compacted_idx = self.storage.get_compacted_idx()?;
        let entries = self.storage.get_entries(0, compact_idx - compacted_idx)?;
        let delta = T::Snapshot::create(entries.as_slice());
        match self.storage.get_snapshot()? {
            Some(mut s) => {
                s.merge(delta);
                Ok(s)
            }
            None => Ok(delta),
        }
    }

    pub(crate) fn create_diff_snapshot(
        &mut self,
        from_idx: u64,
        to_idx: u64,
    ) -> StorageResult<SnapshotType<T>> {
        if self.get_compacted_idx()? >= from_idx {
            Ok(SnapshotType::Complete(self.create_snapshot(to_idx)?))
        } else {
            let diff_entries = self.get_entries(from_idx, to_idx)?;
            Ok(SnapshotType::Delta(T::Snapshot::create(
                diff_entries.as_slice(),
            )))
        }
    }

    /// This operation is atomic, but non-reversible after completion
    pub(crate) fn set_snapshot(&mut self, idx: u64, snapshot: T::Snapshot) -> StorageResult<()> {
        let old_compacted_idx = self.storage.get_compacted_idx()?;
        let old_snapshot = self.storage.get_snapshot()?;
        if idx > old_compacted_idx {
            self.storage.set_compacted_idx(idx)?;
            if let Err(e) = self.storage.set_snapshot(Some(snapshot)) {
                self.storage.set_compacted_idx(old_compacted_idx)?;
                return Err(e);
            }
            if let Err(e) = self.storage.trim(idx - old_compacted_idx) {
                self.storage.set_compacted_idx(old_compacted_idx)?;
                self.storage.set_snapshot(old_snapshot)?;
                return Err(e);
            }
        }
        Ok(())
    }

    pub(crate) fn merge_snapshot(&mut self, idx: u64, delta: T::Snapshot) -> StorageResult<()> {
        let log_len = self.storage.get_log_len()?;
        let mut snapshot = if let Some(snap) = self.storage.get_snapshot()? {
            snap
        } else {
            self.create_snapshot(log_len)?
        };
        snapshot.merge(delta);
        self.set_snapshot(idx, snapshot)
    }

    pub(crate) fn try_trim(&mut self, idx: u64) -> StorageResult<()> {
        let compacted_idx = self.storage.get_compacted_idx()?;
        if idx <= compacted_idx {
            Ok(()) // already trimmed or snapshotted this index.
        } else {
            let decided_idx = self.storage.get_decided_idx()?;
            if idx <= decided_idx {
                self.storage.set_compacted_idx(idx)?;
                if let Err(e) = self.storage.trim(idx - compacted_idx) {
                    self.storage.set_compacted_idx(compacted_idx)?;
                    Err(e)
                } else {
                    Ok(())
                }
            } else {
                Err(CompactionErr::UndecidedIndex(decided_idx))?
            }
        }
    }

    pub(crate) fn get_compacted_idx(&self) -> StorageResult<u64> {
        self.storage.get_compacted_idx()
    }

    pub(crate) fn try_snapshot(&mut self, snapshot_idx: Option<u64>) -> StorageResult<()> {
        let decided_idx = self.get_decided_idx()?;
        let idx = match snapshot_idx {
            Some(i) => {
                if i <= decided_idx {
                    i
                } else {
                    Err(CompactionErr::UndecidedIndex(decided_idx))?
                }
            }
            None => decided_idx,
        };
        if idx > self.get_compacted_idx()? {
            let snapshot = self.create_snapshot(idx)?;
            self.set_snapshot(idx, snapshot)?;
        }
        Ok(())
    }
}
