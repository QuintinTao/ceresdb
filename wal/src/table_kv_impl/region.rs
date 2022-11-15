// Copyright 2022 CeresDB Project Authors. Licensed under Apache-2.0.

//! Region in wal.

use std::{
    cmp,
    convert::TryInto,
    mem,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use common_types::bytes::BytesMut;
use common_util::{define_result, runtime::Runtime};
use log::debug;
use snafu::{ensure, Backtrace, OptionExt, ResultExt, Snafu};
use table_kv::{
    KeyBoundary, ScanContext, ScanIter, ScanRequest, TableError, TableKv, WriteBatch, WriteContext,
};
use tokio::sync::Mutex;

use crate::{
    kv_encoder::{LogEncoding, LogKey},
    log_batch::{LogEntry, LogWriteBatch},
    manager::{self, ReadContext, ReadRequest, RegionId, SequenceNumber, SyncLogIterator},
    table_kv_impl::{encoding, model::RegionEntry, namespace::BucketRef, WalRuntimes},
};

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Failed to get value, key:{}, err:{}", key, source,))]
    GetValue {
        key: String,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[snafu(display("Failed to decode entry, key:{}, err:{}", key, source,))]
    Decode {
        key: String,
        source: crate::table_kv_impl::model::Error,
    },

    #[snafu(display("Failed to encode entry, key:{}, err:{}", key, source,))]
    Encode {
        key: String,
        source: crate::table_kv_impl::model::Error,
    },

    #[snafu(display("Failed to do log codec, err:{}.", source))]
    LogCodec { source: crate::kv_encoder::Error },

    #[snafu(display("Failed to scan table, err:{}", source))]
    Scan {
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[snafu(display("Failed to write value, key:{}, err:{}", key, source))]
    WriteValue {
        key: String,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[snafu(display(
        "Sequence of region overflow, region_id:{}.\nBacktrace:\n{}",
        region_id,
        backtrace
    ))]
    SequenceOverflow {
        region_id: RegionId,
        backtrace: Backtrace,
    },

    #[snafu(display(
        "Failed to write log to table, region_id:{}, err:{}",
        region_id,
        source
    ))]
    WriteLog {
        region_id: RegionId,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[snafu(display(
        "Region not exists, region_id:{}.\nBacktrace:\n{}",
        region_id,
        backtrace
    ))]
    RegionNotExists {
        region_id: RegionId,
        backtrace: Backtrace,
    },

    #[snafu(display("Failed to execute in runtime, err:{}", source))]
    RuntimeExec { source: common_util::runtime::Error },

    #[snafu(display("Failed to delete table, region_id:{}, err:{}", region_id, source))]
    Delete {
        region_id: RegionId,
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

define_result!(Error);

/// Default batch size (100) to clean records.
const DEFAULT_CLEAN_BATCH_SIZE: i32 = 100;

struct RegionState {
    region_id: RegionId,
    /// Start sequence (inclusive) of this region, update is protected by the
    /// `writer` lock.
    start_sequence: AtomicU64,
    /// Last sequence (inclusive) of this region, update is protected by the
    /// `writer` lock.
    last_sequence: AtomicU64,
}

impl RegionState {
    #[inline]
    fn last_sequence(&self) -> SequenceNumber {
        self.last_sequence.load(Ordering::Relaxed)
    }

    #[inline]
    fn start_sequence(&self) -> SequenceNumber {
        self.start_sequence.load(Ordering::Relaxed)
    }

    #[inline]
    fn set_start_sequence(&self, sequence: SequenceNumber) {
        self.start_sequence.store(sequence, Ordering::Relaxed);
    }

    #[inline]
    fn region_entry(&self) -> RegionEntry {
        RegionEntry {
            region_id: self.region_id,
            start_sequence: self.start_sequence.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CleanContext {
    pub scan_timeout: Duration,
    pub batch_size: usize,
}

impl Default for CleanContext {
    fn default() -> Self {
        Self {
            scan_timeout: Duration::from_secs(10),
            batch_size: DEFAULT_CLEAN_BATCH_SIZE as usize,
        }
    }
}

/// Region can be viewed as an append only log file.
pub struct Region {
    runtimes: WalRuntimes,
    state: RegionState,
    writer: Mutex<RegionWriter>,
}

// Async or non-blocking operations.
impl Region {
    /// Open region of given `region_id`, the caller should ensure the meta data
    /// of this region is stored in `region_meta_table`, and the wal log
    /// records are stored in `buckets`.
    pub async fn open<T: TableKv>(
        runtimes: WalRuntimes,
        table_kv: &T,
        scan_ctx: ScanContext,
        region_meta_table: &str,
        region_id: RegionId,
        // Buckets ordered by time.
        buckets: Vec<BucketRef>,
    ) -> Result<Option<Region>> {
        let table_kv = table_kv.clone();
        let region_meta_table = region_meta_table.to_string();
        let rt = runtimes.bg_runtime.clone();

        rt.spawn_blocking(move || {
            // Load of create region entry.
            let region_entry =
                match Self::load_region_entry(&table_kv, &region_meta_table, region_id)? {
                    Some(v) => v,
                    None => return Ok(None),
                };

            // Load last sequence of this region.
            let last_sequence = Self::load_last_sequence(&table_kv, scan_ctx, region_id, &buckets)?;

            Ok(Some(Self {
                runtimes,
                state: RegionState {
                    region_id,
                    start_sequence: AtomicU64::new(region_entry.start_sequence),
                    last_sequence: AtomicU64::new(last_sequence),
                },
                writer: Mutex::new(RegionWriter),
            }))
        })
        .await
        .context(RuntimeExec)?
    }

    /// Similar to `open()`, open region of given `region_id`. If the region is
    /// not exists, insert a new region entry into `region_meta_table`. Only
    /// one writer is allowed to insert the new region entry.
    pub async fn open_or_create<T: TableKv>(
        runtimes: WalRuntimes,
        table_kv: &T,
        scan_ctx: ScanContext,
        region_meta_table: &str,
        region_id: RegionId,
        // Buckets ordered by time.
        buckets: Vec<BucketRef>,
    ) -> Result<Region> {
        let table_kv = table_kv.clone();
        let region_meta_table = region_meta_table.to_string();
        let rt = runtimes.bg_runtime.clone();

        rt.spawn_blocking(move || {
            // Load of create region entry.
            let mut writer = RegionWriter;
            let region_entry =
                match Self::load_region_entry(&table_kv, &region_meta_table, region_id)? {
                    Some(v) => v,
                    None => {
                        let entry = RegionEntry::new(region_id);
                        writer.insert_or_load_region_entry(&table_kv, &region_meta_table, entry)?
                    }
                };

            // Load last sequence of this region.
            let last_sequence = Self::load_last_sequence(&table_kv, scan_ctx, region_id, &buckets)?;

            Ok(Self {
                runtimes,
                state: RegionState {
                    region_id,
                    start_sequence: AtomicU64::new(region_entry.start_sequence),
                    last_sequence: AtomicU64::new(last_sequence),
                },
                writer: Mutex::new(writer),
            })
        })
        .await
        .context(RuntimeExec)?
    }

    pub async fn write_log<T: TableKv>(
        &self,
        table_kv: &T,
        bucket: &BucketRef,
        ctx: &manager::WriteContext,
        log_batch: &LogWriteBatch,
    ) -> Result<SequenceNumber> {
        let mut writer = self.writer.lock().await;
        writer
            .write_log(
                &self.runtimes.write_runtime,
                table_kv,
                &self.state,
                bucket,
                ctx,
                log_batch,
            )
            .await
    }

    pub async fn read_log<T: TableKv>(
        &self,
        table_kv: &T,
        buckets: Vec<BucketRef>,
        ctx: &ReadContext,
        request: &ReadRequest,
    ) -> Result<TableLogIterator<T>> {
        // Prepare start/end sequence to read, now this doesn't provide snapshot
        // isolation semantics since delete and write operations may happen
        // during reading start/end sequence.
        let start_sequence = match request.start.as_start_sequence_number() {
            Some(request_start_sequence) => {
                let region_start_sequence = self.state.start_sequence();
                // Avoid reading deleted log entries.
                cmp::max(region_start_sequence, request_start_sequence)
            }
            None => return Ok(TableLogIterator::new_empty(table_kv.clone())),
        };
        let end_sequence = match request.end.as_end_sequence_number() {
            Some(request_end_sequence) => {
                let region_last_sequence = self.state.last_sequence();
                // Avoid reading entries newer than current last sequence.
                cmp::min(region_last_sequence, request_end_sequence)
            }
            None => return Ok(TableLogIterator::new_empty(table_kv.clone())),
        };

        let min_log_key = (self.state.region_id, start_sequence);
        let max_log_key = (self.state.region_id, end_sequence);

        let scan_ctx = ScanContext {
            timeout: ctx.timeout,
            ..Default::default()
        };

        Ok(TableLogIterator::new(
            buckets,
            min_log_key,
            max_log_key,
            scan_ctx,
            table_kv.clone(),
        ))
    }

    pub async fn delete_entries_up_to<T: TableKv>(
        &self,
        table_kv: &T,
        region_meta_table: &str,
        sequence_num: SequenceNumber,
    ) -> Result<()> {
        let mut writer = self.writer.lock().await;
        writer
            .delete_entries_up_to(
                &self.runtimes.write_runtime,
                table_kv,
                &self.state,
                region_meta_table,
                sequence_num,
            )
            .await
    }

    #[inline]
    pub fn region_id(&self) -> RegionId {
        self.state.region_id
    }

    #[inline]
    pub fn last_sequence(&self) -> SequenceNumber {
        self.state.last_sequence()
    }
}

// Blocking operations:
impl Region {
    fn load_region_entry<T: TableKv>(
        table_kv: &T,
        region_meta_table: &str,
        region_id: RegionId,
    ) -> Result<Option<RegionEntry>> {
        let key = encoding::format_region_key(region_id);
        table_kv
            .get(region_meta_table, key.as_bytes())
            .map_err(|e| Box::new(e) as _)
            .context(GetValue { key: &key })?
            .map(|value| RegionEntry::decode(&value).context(Decode { key }))
            .transpose()
    }

    // TODO(yingwen): We can cache last sequence of several buckets (be sure not to
    // leak bucekts that has been deleted).
    fn load_last_sequence<T: TableKv>(
        table_kv: &T,
        scan_ctx: ScanContext,
        region_id: RegionId,
        buckets: &[BucketRef],
    ) -> Result<SequenceNumber> {
        // Starts from the latest bucket, find last sequence of given region id.
        let mut last_sequence = common_types::MIN_SEQUENCE_NUMBER;
        for bucket in buckets.iter().rev() {
            let table_name = bucket.wal_shard_table(region_id);

            if let Some(seq) = Self::load_last_sequence_from_table(
                table_kv,
                scan_ctx.clone(),
                table_name,
                region_id,
            )? {
                last_sequence = seq;
            }
        }

        Ok(last_sequence)
    }

    fn load_last_sequence_from_table<T: TableKv>(
        table_kv: &T,
        scan_ctx: ScanContext,
        table_name: &str,
        region_id: RegionId,
    ) -> Result<Option<SequenceNumber>> {
        let log_encoding = LogEncoding::newest();
        let mut encode_buf = BytesMut::new();

        let start_log_key = (region_id, common_types::MIN_SEQUENCE_NUMBER);
        log_encoding
            .encode_key(&mut encode_buf, &start_log_key)
            .context(LogCodec)?;
        let scan_start = KeyBoundary::included(&encode_buf);

        encode_buf.clear();
        let end_log_key = (region_id, common_types::MAX_SEQUENCE_NUMBER);
        log_encoding
            .encode_key(&mut encode_buf, &end_log_key)
            .context(LogCodec)?;
        let scan_end = KeyBoundary::included(&encode_buf);

        let scan_req = ScanRequest {
            start: scan_start,
            end: scan_end,
            // We need to find the maximum sequence number.
            reverse: true,
        };

        let iter = table_kv
            .scan(scan_ctx, table_name, scan_req)
            .map_err(|e| Box::new(e) as _)
            .context(Scan)?;

        if !iter.valid() {
            return Ok(None);
        }

        if !log_encoding.is_log_key(iter.key()).context(LogCodec)? {
            return Ok(None);
        }

        let log_key = log_encoding.decode_key(iter.key()).context(LogCodec)?;

        Ok(Some(log_key.1))
    }

    pub fn clean_deleted_logs<T: TableKv>(
        &self,
        table_kv: &T,
        ctx: &CleanContext,
        buckets: &[BucketRef],
    ) -> Result<()> {
        // Inclusive min log key.
        let min_log_key = (self.state.region_id, common_types::MIN_SEQUENCE_NUMBER);
        // Exlusive max log key.
        let max_log_key = (self.state.region_id, self.state.start_sequence());

        let mut seek_key_buf = BytesMut::new();
        let log_encoding = LogEncoding::newest();
        log_encoding
            .encode_key(&mut seek_key_buf, &min_log_key)
            .context(LogCodec)?;
        let start = KeyBoundary::included(&seek_key_buf);
        log_encoding
            .encode_key(&mut seek_key_buf, &max_log_key)
            .context(LogCodec)?;
        // We should not clean record with start sequence, so we use exclusive boundary.
        let end = KeyBoundary::excluded(&seek_key_buf);

        let scan_req = ScanRequest {
            start,
            end,
            reverse: false,
        };
        let scan_ctx = ScanContext {
            timeout: ctx.scan_timeout,
            batch_size: ctx
                .batch_size
                .try_into()
                .unwrap_or(DEFAULT_CLEAN_BATCH_SIZE),
        };

        for bucket in buckets {
            let table_name = bucket.wal_shard_table(self.state.region_id);
            let iter = table_kv
                .scan(scan_ctx.clone(), table_name, scan_req.clone())
                .map_err(|e| Box::new(e) as _)
                .context(Scan)?;

            self.clean_logs_from_iter(table_kv, ctx, table_name, iter)?;
        }

        Ok(())
    }

    fn clean_logs_from_iter<T: TableKv>(
        &self,
        table_kv: &T,
        ctx: &CleanContext,
        table_name: &str,
        mut iter: T::ScanIter,
    ) -> Result<()> {
        let mut write_batch = T::WriteBatch::with_capacity(ctx.batch_size);
        let (mut write_batch_size, mut total_deleted) = (0, 0);
        while iter.valid() {
            write_batch.delete(iter.key());
            write_batch_size += 1;
            total_deleted += 1;

            if write_batch_size >= ctx.batch_size {
                let wb = mem::replace(
                    &mut write_batch,
                    T::WriteBatch::with_capacity(ctx.batch_size),
                );
                write_batch_size = 0;
                table_kv
                    .write(WriteContext::default(), table_name, wb)
                    .map_err(|e| Box::new(e) as _)
                    .context(Delete {
                        region_id: self.state.region_id,
                    })?;
            }

            let has_next = iter.next().map_err(|e| Box::new(e) as _).context(Scan)?;
            if !has_next {
                let wb = mem::replace(
                    &mut write_batch,
                    T::WriteBatch::with_capacity(ctx.batch_size),
                );
                table_kv
                    .write(WriteContext::default(), table_name, wb)
                    .map_err(|e| Box::new(e) as _)
                    .context(Delete {
                        region_id: self.state.region_id,
                    })?;

                break;
            }
        }

        if total_deleted > 0 {
            debug!(
                "Clean logs of region, region_id:{}, table_name:{}, total_deleted:{}",
                self.state.region_id, table_name, total_deleted
            );
        }

        Ok(())
    }
}

pub type RegionRef = Arc<Region>;

#[derive(Debug)]
pub struct TableLogIterator<T: TableKv> {
    buckets: Vec<BucketRef>,
    /// Inclusive max log key.
    max_log_key: LogKey,
    scan_ctx: ScanContext,
    table_kv: T,

    current_log_key: LogKey,
    // The iterator is exhausted if `current_bucket_index >= bucets.size()`.
    current_bucket_index: usize,
    // The `current_iter` should be either a valid iterator or None.
    current_iter: Option<T::ScanIter>,
    log_encoding: LogEncoding,
    // TODO(ygf11): Remove this after issue#120 is resolved.
    previous_value: Vec<u8>,
}

impl<T: TableKv> TableLogIterator<T> {
    pub fn new_empty(table_kv: T) -> Self {
        Self {
            buckets: Vec::new(),
            max_log_key: (0, 0),
            scan_ctx: ScanContext::default(),
            table_kv,
            current_log_key: (0, 0),
            current_bucket_index: 0,
            current_iter: None,
            log_encoding: LogEncoding::newest(),
            previous_value: Vec::default(),
        }
    }

    fn new(
        buckets: Vec<BucketRef>,
        min_log_key: LogKey,
        max_log_key: LogKey,
        scan_ctx: ScanContext,
        table_kv: T,
    ) -> Self {
        TableLogIterator {
            buckets,
            max_log_key,
            scan_ctx,
            table_kv,
            current_log_key: min_log_key,
            current_bucket_index: 0,
            current_iter: None,
            log_encoding: LogEncoding::newest(),
            previous_value: Vec::default(),
        }
    }

    #[inline]
    fn no_more_data(&self) -> bool {
        self.current_bucket_index >= self.buckets.len() || self.current_log_key > self.max_log_key
    }

    fn new_scan_request(&self) -> Result<ScanRequest> {
        let mut seek_key_buf = BytesMut::new();
        self.log_encoding
            .encode_key(&mut seek_key_buf, &self.current_log_key)
            .context(LogCodec)?;
        let start = KeyBoundary::included(&seek_key_buf);
        self.log_encoding
            .encode_key(&mut seek_key_buf, &self.max_log_key)
            .context(LogCodec)?;
        let end = KeyBoundary::included(&seek_key_buf);

        Ok(ScanRequest {
            start,
            end,
            reverse: false,
        })
    }

    /// Scan buckets to find next valid iterator, returns true if such iterator
    /// has been found.
    fn scan_buckets(&mut self) -> Result<bool> {
        let region_id = self.max_log_key.0;
        let scan_req = self.new_scan_request()?;

        while self.current_bucket_index < self.buckets.len() {
            if self.current_bucket_index > 0 {
                assert!(
                    self.buckets[self.current_bucket_index - 1].gmt_start_ms()
                        < self.buckets[self.current_bucket_index].gmt_start_ms()
                );
            }

            let table_name = self.buckets[self.current_bucket_index].wal_shard_table(region_id);
            let iter = self
                .table_kv
                .scan(self.scan_ctx.clone(), table_name, scan_req.clone())
                .map_err(|e| Box::new(e) as _)
                .context(Scan)?;
            if iter.valid() {
                self.current_iter = Some(iter);
                return Ok(true);
            }

            self.current_bucket_index += 1;
        }

        Ok(false)
    }

    fn step_current_iter(&mut self) -> Result<()> {
        if let Some(iter) = &mut self.current_iter {
            if !iter.next().map_err(|e| Box::new(e) as _).context(Scan)? {
                self.current_iter = None;
                self.current_bucket_index += 1;
            }
        }

        Ok(())
    }
}

impl<T: TableKv> SyncLogIterator for TableLogIterator<T> {
    fn next_log_entry(&mut self) -> manager::Result<Option<LogEntry<&'_ [u8]>>> {
        if self.no_more_data() {
            return Ok(None);
        }

        // If `current_iter` is None, scan from current to last bucket util we get a
        // valid iterator.
        if self.current_iter.is_none() {
            let has_valid_iter = self
                .scan_buckets()
                .map_err(|e| Box::new(e) as _)
                .context(manager::Read)?;
            if !has_valid_iter {
                assert!(self.no_more_data());
                return Ok(None);
            }
        }

        // Fetch and decode current log entry.
        let current_iter = self.current_iter.as_ref().unwrap();
        self.current_log_key = self
            .log_encoding
            .decode_key(current_iter.key())
            .map_err(|e| Box::new(e) as _)
            .context(manager::Decoding)?;
        let payload = self
            .log_encoding
            .decode_value(current_iter.value())
            .map_err(|e| Box::new(e) as _)
            .context(manager::Encoding)?;

        // To unblock pr#119, we use the following to simple resolve borrow-check error.
        // detail info: https://github.com/CeresDB/ceresdb/issues/120
        self.previous_value = payload.to_owned();

        // Step current iterator, if it becomes invalid, reset `current_iter` to None
        // and advance `current_bucket_index`.
        self.step_current_iter()
            .map_err(|e| Box::new(e) as _)
            .context(manager::Read)?;

        let log_entry = LogEntry {
            table_id: self.current_log_key.0,
            sequence: self.current_log_key.1,
            payload: self.previous_value.as_slice(),
        };

        Ok(Some(log_entry))
    }
}

struct RegionWriter;

// Blocking operations.
impl RegionWriter {
    fn insert_or_load_region_entry<T: TableKv>(
        &mut self,
        table_kv: &T,
        region_meta_table: &str,
        region_entry: RegionEntry,
    ) -> Result<RegionEntry> {
        let key = encoding::format_region_key(region_entry.region_id);
        let value = region_entry.encode().context(Encode { key: &key })?;

        let mut batch = T::WriteBatch::default();
        batch.insert(key.as_bytes(), &value);

        let res = table_kv.write(WriteContext::default(), region_meta_table, batch);

        match &res {
            Ok(()) => Ok(region_entry),
            Err(e) => {
                if e.is_primary_key_duplicate() {
                    Region::load_region_entry(table_kv, region_meta_table, region_entry.region_id)?
                        .context(RegionNotExists {
                            region_id: region_entry.region_id,
                        })
                } else {
                    res.map_err(|e| Box::new(e) as _)
                        .context(WriteValue { key: &key })?;

                    Ok(region_entry)
                }
            }
        }
    }

    fn update_region_entry<T: TableKv>(
        table_kv: &T,
        region_meta_table: &str,
        region_entry: &RegionEntry,
    ) -> Result<()> {
        let key = encoding::format_region_key(region_entry.region_id);
        let value = region_entry.encode().context(Encode { key: &key })?;

        let mut batch = T::WriteBatch::default();
        batch.insert_or_update(key.as_bytes(), &value);

        table_kv
            .write(WriteContext::default(), region_meta_table, batch)
            .map_err(|e| Box::new(e) as _)
            .context(WriteValue { key: &key })
    }

    /// Allocate a continuous range of [SequenceNumber] and returns the starts
    /// [SequenceNumber] of the range [start, start + `number`].
    fn alloc_sequence_num(
        &mut self,
        region_state: &RegionState,
        number: u64,
    ) -> Result<SequenceNumber> {
        ensure!(
            region_state.last_sequence() < common_types::MAX_SEQUENCE_NUMBER,
            SequenceOverflow {
                region_id: region_state.region_id,
            }
        );

        let last_sequence = region_state
            .last_sequence
            .fetch_add(number, Ordering::Relaxed);
        Ok(last_sequence + 1)
    }
}

impl RegionWriter {
    async fn write_log<T: TableKv>(
        &mut self,
        runtime: &Runtime,
        table_kv: &T,
        region_state: &RegionState,
        bucket: &BucketRef,
        ctx: &manager::WriteContext,
        log_batch: &LogWriteBatch,
    ) -> Result<SequenceNumber> {
        debug!(
            "Wal region begin writing, ctx:{:?}, region_id:{}, log_entries_num:{}",
            ctx,
            log_batch.location.table_id,
            log_batch.entries.len()
        );

        let log_encoding = LogEncoding::newest();
        let entries_num = log_batch.len() as u64;
        let (wb, max_sequence_num) = {
            let mut wb = T::WriteBatch::with_capacity(log_batch.len());
            let mut next_sequence_num = self.alloc_sequence_num(region_state, entries_num)?;
            let mut key_buf = BytesMut::new();

            for entry in &log_batch.entries {
                log_encoding
                    .encode_key(
                        &mut key_buf,
                        &(log_batch.location.table_id, next_sequence_num),
                    )
                    .context(LogCodec)?;
                wb.insert(&key_buf, &entry.payload);

                next_sequence_num += 1;
            }

            (wb, next_sequence_num - 1)
        };

        let table_kv = table_kv.clone();
        let bucket = bucket.clone();
        let region_id = log_batch.location.table_id;
        runtime
            .spawn_blocking(move || {
                let table_name = bucket.wal_shard_table(region_id);
                table_kv
                    .write(WriteContext::default(), table_name, wb)
                    .map_err(|e| Box::new(e) as _)
                    .context(WriteLog { region_id })
            })
            .await
            .context(RuntimeExec)??;

        Ok(max_sequence_num)
    }

    /// Delete entries in the range `[0, sequence_num]`.
    ///
    /// The delete procedure is ensured to be sequential.
    async fn delete_entries_up_to<T: TableKv>(
        &mut self,
        runtime: &Runtime,
        table_kv: &T,
        region_state: &RegionState,
        region_meta_table: &str,
        mut sequence_num: SequenceNumber,
    ) -> Result<()> {
        debug!(
            "Try to delete entries, region_id:{}, sequence_num:{}",
            region_state.region_id, sequence_num
        );

        ensure!(
            sequence_num < common_types::MAX_SEQUENCE_NUMBER,
            SequenceOverflow {
                region_id: region_state.region_id,
            }
        );

        let last_sequence = region_state.last_sequence();
        if sequence_num > last_sequence {
            sequence_num = last_sequence;
        }

        // Update min_sequence.
        let mut region_entry = region_state.region_entry();
        if region_entry.start_sequence <= sequence_num {
            region_entry.start_sequence = sequence_num + 1;
        }

        debug!(
            "Update region entry due to deletion, table:{}, region_entry:{:?}",
            region_meta_table, region_entry
        );

        let table_kv = table_kv.clone();
        let region_meta_table = region_meta_table.to_string();
        runtime
            .spawn_blocking(move || {
                // Persist modification to region meta table.
                Self::update_region_entry(&table_kv, &region_meta_table, &region_entry)
            })
            .await
            .context(RuntimeExec)??;

        // Update region state in memory.
        region_state.set_start_sequence(region_entry.start_sequence);

        Ok(())
    }
}
