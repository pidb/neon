//!
//! WAL redo. This service runs PostgreSQL in a special wal_redo mode
//! to apply given WAL records over an old page image and return new
//! page image.
//!
//! We rely on Postgres to perform WAL redo for us. We launch a
//! postgres process in special "wal redo" mode that's similar to
//! single-user mode. We then pass the previous page image, if any,
//! and all the WAL records we want to apply, to the postgres
//! process. Then we get the page image back. Communication with the
//! postgres process happens via stdin/stdout
//!
//! See pgxn/neon_walredo/walredoproc.c for the other side of
//! this communication.
//!
//! The Postgres process is assumed to be secure against malicious WAL
//! records. It achieves it by dropping privileges before replaying
//! any WAL records, so that even if an attacker hijacks the Postgres
//! process, he cannot escape out of it.
//!
use byteorder::{ByteOrder, LittleEndian};
use bytes::{BufMut, Bytes, BytesMut};
use nix::poll::*;
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::prelude::*;
use std::io::{Error, ErrorKind};
use std::ops::{Deref, DerefMut};
use std::os::unix::io::AsRawFd;
use std::os::unix::prelude::CommandExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;
use std::{fs, io};
use tracing::*;
use utils::crashsafe::path_with_suffix_extension;
use utils::{bin_ser::BeSer, id::TenantId, lsn::Lsn, nonblock::set_nonblock};

use crate::metrics::{
    WAL_REDO_BYTES_HISTOGRAM, WAL_REDO_RECORDS_HISTOGRAM, WAL_REDO_RECORD_COUNTER, WAL_REDO_TIME,
    WAL_REDO_WAIT_TIME,
};
use crate::pgdatadir_mapping::{key_to_rel_block, key_to_slru_block};
use crate::repository::Key;
use crate::task_mgr::BACKGROUND_RUNTIME;
use crate::walrecord::NeonWalRecord;
use crate::{config::PageServerConf, TEMP_FILE_SUFFIX};
use pageserver_api::reltag::{RelTag, SlruKind};
use postgres_ffi::pg_constants;
use postgres_ffi::relfile_utils::VISIBILITYMAP_FORKNUM;
use postgres_ffi::v14::nonrelfile_utils::{
    mx_offset_to_flags_bitshift, mx_offset_to_flags_offset, mx_offset_to_member_offset,
    transaction_id_set_status,
};
use postgres_ffi::BLCKSZ;

///
/// `RelTag` + block number (`blknum`) gives us a unique id of the page in the cluster.
///
/// In Postgres `BufferTag` structure is used for exactly the same purpose.
/// [See more related comments here](https://github.com/postgres/postgres/blob/99c5852e20a0987eca1c38ba0c09329d4076b6a0/src/include/storage/buf_internals.h#L91).
///
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Serialize)]
pub struct BufferTag {
    pub rel: RelTag,
    pub blknum: u32,
}

///
/// WAL Redo Manager is responsible for replaying WAL records.
///
/// Callers use the WAL redo manager through this abstract interface,
/// which makes it easy to mock it in tests.
pub trait WalRedoManager: Send + Sync {
    /// Apply some WAL records.
    ///
    /// The caller passes an old page image, and WAL records that should be
    /// applied over it. The return value is a new page image, after applying
    /// the reords.
    fn request_redo(
        &self,
        key: Key,
        lsn: Lsn,
        base_img: Option<(Lsn, Bytes)>,
        records: Vec<(Lsn, NeonWalRecord)>,
        pg_version: u32,
    ) -> Result<Bytes, WalRedoError>;
}

///
/// This is the real implementation that uses a Postgres process to
/// perform WAL replay. Only one thread can use the process at a time,
/// that is controlled by the Mutex. In the future, we might want to
/// launch a pool of processes to allow concurrent replay of multiple
/// records.
///
pub struct PostgresRedoManager {
    tenant_id: TenantId,
    conf: &'static PageServerConf,

    process: Mutex<Option<PostgresRedoProcess>>,
}

/// Can this request be served by neon redo functions
/// or we need to pass it to wal-redo postgres process?
fn can_apply_in_neon(rec: &NeonWalRecord) -> bool {
    // Currently, we don't have bespoken Rust code to replay any
    // Postgres WAL records. But everything else is handled in neon.
    #[allow(clippy::match_like_matches_macro)]
    match rec {
        NeonWalRecord::Postgres {
            will_init: _,
            rec: _,
        } => false,
        _ => true,
    }
}

/// An error happened in WAL redo
#[derive(Debug, thiserror::Error)]
pub enum WalRedoError {
    #[error(transparent)]
    IoError(#[from] std::io::Error),

    #[error("cannot perform WAL redo now")]
    InvalidState,
    #[error("cannot perform WAL redo for this request")]
    InvalidRequest,
    #[error("cannot perform WAL redo for this record")]
    InvalidRecord,
}

///
/// Public interface of WAL redo manager
///
impl WalRedoManager for PostgresRedoManager {
    ///
    /// Request the WAL redo manager to apply some WAL records
    ///
    /// The WAL redo is handled by a separate thread, so this just sends a request
    /// to the thread and waits for response.
    ///
    fn request_redo(
        &self,
        key: Key,
        lsn: Lsn,
        base_img: Option<(Lsn, Bytes)>,
        records: Vec<(Lsn, NeonWalRecord)>,
        pg_version: u32,
    ) -> Result<Bytes, WalRedoError> {
        if records.is_empty() {
            error!("invalid WAL redo request with no records");
            return Err(WalRedoError::InvalidRequest);
        }

        let base_img_lsn = base_img.as_ref().map(|p| p.0).unwrap_or(Lsn::INVALID);
        let mut img = base_img.map(|p| p.1);
        let mut batch_neon = can_apply_in_neon(&records[0].1);
        let mut batch_start = 0;
        for i in 1..records.len() {
            let rec_neon = can_apply_in_neon(&records[i].1);

            if rec_neon != batch_neon {
                let result = if batch_neon {
                    self.apply_batch_neon(key, lsn, img, &records[batch_start..i])
                } else {
                    self.apply_batch_postgres(
                        key,
                        lsn,
                        img,
                        base_img_lsn,
                        &records[batch_start..i],
                        self.conf.wal_redo_timeout,
                        pg_version,
                    )
                };
                img = Some(result?);

                batch_neon = rec_neon;
                batch_start = i;
            }
        }
        // last batch
        if batch_neon {
            self.apply_batch_neon(key, lsn, img, &records[batch_start..])
        } else {
            self.apply_batch_postgres(
                key,
                lsn,
                img,
                base_img_lsn,
                &records[batch_start..],
                self.conf.wal_redo_timeout,
                pg_version,
            )
        }
    }
}

impl PostgresRedoManager {
    ///
    /// Create a new PostgresRedoManager.
    ///
    pub fn new(conf: &'static PageServerConf, tenant_id: TenantId) -> PostgresRedoManager {
        // The actual process is launched lazily, on first request.
        PostgresRedoManager {
            tenant_id,
            conf,
            process: Mutex::new(None),
        }
    }

    /// Launch process pre-emptively. Should not be needed except for benchmarking.
    pub fn launch_process(&mut self, pg_version: u32) -> anyhow::Result<()> {
        let inner = self.process.get_mut().unwrap();
        if inner.is_none() {
            let p = PostgresRedoProcess::launch(self.conf, self.tenant_id, pg_version)?;
            *inner = Some(p);
        }
        Ok(())
    }

    ///
    /// Process one request for WAL redo using wal-redo postgres
    ///
    #[allow(clippy::too_many_arguments)]
    fn apply_batch_postgres(
        &self,
        key: Key,
        lsn: Lsn,
        base_img: Option<Bytes>,
        base_img_lsn: Lsn,
        records: &[(Lsn, NeonWalRecord)],
        wal_redo_timeout: Duration,
        pg_version: u32,
    ) -> Result<Bytes, WalRedoError> {
        let (rel, blknum) = key_to_rel_block(key).or(Err(WalRedoError::InvalidRecord))?;

        let start_time = Instant::now();

        let mut process_guard = self.process.lock().unwrap();
        let lock_time = Instant::now();

        // launch the WAL redo process on first use
        if process_guard.is_none() {
            let p = PostgresRedoProcess::launch(self.conf, self.tenant_id, pg_version)?;
            *process_guard = Some(p);
        }
        let process = process_guard.as_mut().unwrap();

        WAL_REDO_WAIT_TIME.observe(lock_time.duration_since(start_time).as_secs_f64());

        // Relational WAL records are applied using wal-redo-postgres
        let buf_tag = BufferTag { rel, blknum };
        let result = process
            .apply_wal_records(buf_tag, base_img, records, wal_redo_timeout)
            .map_err(WalRedoError::IoError);

        let end_time = Instant::now();
        let duration = end_time.duration_since(lock_time);

        let len = records.len();
        let nbytes = records.iter().fold(0, |acumulator, record| {
            acumulator
                + match &record.1 {
                    NeonWalRecord::Postgres { rec, .. } => rec.len(),
                    _ => unreachable!("Only PostgreSQL records are accepted in this batch"),
                }
        });

        WAL_REDO_TIME.observe(duration.as_secs_f64());
        WAL_REDO_RECORDS_HISTOGRAM.observe(len as f64);
        WAL_REDO_BYTES_HISTOGRAM.observe(nbytes as f64);

        debug!(
            "postgres applied {} WAL records ({} bytes) in {} us to reconstruct page image at LSN {}",
            len,
            nbytes,
            duration.as_micros(),
            lsn
        );

        // If something went wrong, don't try to reuse the process. Kill it, and
        // next request will launch a new one.
        if result.is_err() {
            error!(
                "error applying {} WAL records {}..{} ({} bytes) to base image with LSN {} to reconstruct page image at LSN {}",
                records.len(),
				records.first().map(|p| p.0).unwrap_or(Lsn(0)),
				records.last().map(|p| p.0).unwrap_or(Lsn(0)),
                nbytes,
				base_img_lsn,
                lsn
            );
            let process = process_guard.take().unwrap();
            process.kill();
        }
        result
    }

    ///
    /// Process a batch of WAL records using bespoken Neon code.
    ///
    fn apply_batch_neon(
        &self,
        key: Key,
        lsn: Lsn,
        base_img: Option<Bytes>,
        records: &[(Lsn, NeonWalRecord)],
    ) -> Result<Bytes, WalRedoError> {
        let start_time = Instant::now();

        let mut page = BytesMut::new();
        if let Some(fpi) = base_img {
            // If full-page image is provided, then use it...
            page.extend_from_slice(&fpi[..]);
        } else {
            // All the current WAL record types that we can handle require a base image.
            error!("invalid neon WAL redo request with no base image");
            return Err(WalRedoError::InvalidRequest);
        }

        // Apply all the WAL records in the batch
        for (record_lsn, record) in records.iter() {
            self.apply_record_neon(key, &mut page, *record_lsn, record)?;
        }
        // Success!
        let end_time = Instant::now();
        let duration = end_time.duration_since(start_time);
        WAL_REDO_TIME.observe(duration.as_secs_f64());

        debug!(
            "neon applied {} WAL records in {} ms to reconstruct page image at LSN {}",
            records.len(),
            duration.as_micros(),
            lsn
        );

        Ok(page.freeze())
    }

    fn apply_record_neon(
        &self,
        key: Key,
        page: &mut BytesMut,
        _record_lsn: Lsn,
        record: &NeonWalRecord,
    ) -> Result<(), WalRedoError> {
        match record {
            NeonWalRecord::Postgres {
                will_init: _,
                rec: _,
            } => {
                error!("tried to pass postgres wal record to neon WAL redo");
                return Err(WalRedoError::InvalidRequest);
            }
            NeonWalRecord::ClearVisibilityMapFlags {
                new_heap_blkno,
                old_heap_blkno,
                flags,
            } => {
                // sanity check that this is modifying the correct relation
                let (rel, blknum) = key_to_rel_block(key).or(Err(WalRedoError::InvalidRecord))?;
                assert!(
                    rel.forknum == VISIBILITYMAP_FORKNUM,
                    "ClearVisibilityMapFlags record on unexpected rel {}",
                    rel
                );
                if let Some(heap_blkno) = *new_heap_blkno {
                    // Calculate the VM block and offset that corresponds to the heap block.
                    let map_block = pg_constants::HEAPBLK_TO_MAPBLOCK(heap_blkno);
                    let map_byte = pg_constants::HEAPBLK_TO_MAPBYTE(heap_blkno);
                    let map_offset = pg_constants::HEAPBLK_TO_OFFSET(heap_blkno);

                    // Check that we're modifying the correct VM block.
                    assert!(map_block == blknum);

                    // equivalent to PageGetContents(page)
                    let map = &mut page[pg_constants::MAXALIGN_SIZE_OF_PAGE_HEADER_DATA..];

                    map[map_byte as usize] &= !(flags << map_offset);
                }

                // Repeat for 'old_heap_blkno', if any
                if let Some(heap_blkno) = *old_heap_blkno {
                    let map_block = pg_constants::HEAPBLK_TO_MAPBLOCK(heap_blkno);
                    let map_byte = pg_constants::HEAPBLK_TO_MAPBYTE(heap_blkno);
                    let map_offset = pg_constants::HEAPBLK_TO_OFFSET(heap_blkno);

                    assert!(map_block == blknum);

                    let map = &mut page[pg_constants::MAXALIGN_SIZE_OF_PAGE_HEADER_DATA..];

                    map[map_byte as usize] &= !(flags << map_offset);
                }
            }
            // Non-relational WAL records are handled here, with custom code that has the
            // same effects as the corresponding Postgres WAL redo function.
            NeonWalRecord::ClogSetCommitted { xids, timestamp } => {
                let (slru_kind, segno, blknum) =
                    key_to_slru_block(key).or(Err(WalRedoError::InvalidRecord))?;
                assert_eq!(
                    slru_kind,
                    SlruKind::Clog,
                    "ClogSetCommitted record with unexpected key {}",
                    key
                );
                for &xid in xids {
                    let pageno = xid as u32 / pg_constants::CLOG_XACTS_PER_PAGE;
                    let expected_segno = pageno / pg_constants::SLRU_PAGES_PER_SEGMENT;
                    let expected_blknum = pageno % pg_constants::SLRU_PAGES_PER_SEGMENT;

                    // Check that we're modifying the correct CLOG block.
                    assert!(
                        segno == expected_segno,
                        "ClogSetCommitted record for XID {} with unexpected key {}",
                        xid,
                        key
                    );
                    assert!(
                        blknum == expected_blknum,
                        "ClogSetCommitted record for XID {} with unexpected key {}",
                        xid,
                        key
                    );

                    transaction_id_set_status(
                        xid,
                        pg_constants::TRANSACTION_STATUS_COMMITTED,
                        page,
                    );
                }

                // Append the timestamp
                if page.len() == BLCKSZ as usize + 8 {
                    page.truncate(BLCKSZ as usize);
                }
                if page.len() == BLCKSZ as usize {
                    page.extend_from_slice(&timestamp.to_be_bytes());
                } else {
                    warn!(
                        "CLOG blk {} in seg {} has invalid size {}",
                        blknum,
                        segno,
                        page.len()
                    );
                }
            }
            NeonWalRecord::ClogSetAborted { xids } => {
                let (slru_kind, segno, blknum) =
                    key_to_slru_block(key).or(Err(WalRedoError::InvalidRecord))?;
                assert_eq!(
                    slru_kind,
                    SlruKind::Clog,
                    "ClogSetAborted record with unexpected key {}",
                    key
                );
                for &xid in xids {
                    let pageno = xid as u32 / pg_constants::CLOG_XACTS_PER_PAGE;
                    let expected_segno = pageno / pg_constants::SLRU_PAGES_PER_SEGMENT;
                    let expected_blknum = pageno % pg_constants::SLRU_PAGES_PER_SEGMENT;

                    // Check that we're modifying the correct CLOG block.
                    assert!(
                        segno == expected_segno,
                        "ClogSetAborted record for XID {} with unexpected key {}",
                        xid,
                        key
                    );
                    assert!(
                        blknum == expected_blknum,
                        "ClogSetAborted record for XID {} with unexpected key {}",
                        xid,
                        key
                    );

                    transaction_id_set_status(xid, pg_constants::TRANSACTION_STATUS_ABORTED, page);
                }
            }
            NeonWalRecord::MultixactOffsetCreate { mid, moff } => {
                let (slru_kind, segno, blknum) =
                    key_to_slru_block(key).or(Err(WalRedoError::InvalidRecord))?;
                assert_eq!(
                    slru_kind,
                    SlruKind::MultiXactOffsets,
                    "MultixactOffsetCreate record with unexpected key {}",
                    key
                );
                // Compute the block and offset to modify.
                // See RecordNewMultiXact in PostgreSQL sources.
                let pageno = mid / pg_constants::MULTIXACT_OFFSETS_PER_PAGE as u32;
                let entryno = mid % pg_constants::MULTIXACT_OFFSETS_PER_PAGE as u32;
                let offset = (entryno * 4) as usize;

                // Check that we're modifying the correct multixact-offsets block.
                let expected_segno = pageno / pg_constants::SLRU_PAGES_PER_SEGMENT;
                let expected_blknum = pageno % pg_constants::SLRU_PAGES_PER_SEGMENT;
                assert!(
                    segno == expected_segno,
                    "MultiXactOffsetsCreate record for multi-xid {} with unexpected key {}",
                    mid,
                    key
                );
                assert!(
                    blknum == expected_blknum,
                    "MultiXactOffsetsCreate record for multi-xid {} with unexpected key {}",
                    mid,
                    key
                );

                LittleEndian::write_u32(&mut page[offset..offset + 4], *moff);
            }
            NeonWalRecord::MultixactMembersCreate { moff, members } => {
                let (slru_kind, segno, blknum) =
                    key_to_slru_block(key).or(Err(WalRedoError::InvalidRecord))?;
                assert_eq!(
                    slru_kind,
                    SlruKind::MultiXactMembers,
                    "MultixactMembersCreate record with unexpected key {}",
                    key
                );
                for (i, member) in members.iter().enumerate() {
                    let offset = moff + i as u32;

                    // Compute the block and offset to modify.
                    // See RecordNewMultiXact in PostgreSQL sources.
                    let pageno = offset / pg_constants::MULTIXACT_MEMBERS_PER_PAGE as u32;
                    let memberoff = mx_offset_to_member_offset(offset);
                    let flagsoff = mx_offset_to_flags_offset(offset);
                    let bshift = mx_offset_to_flags_bitshift(offset);

                    // Check that we're modifying the correct multixact-members block.
                    let expected_segno = pageno / pg_constants::SLRU_PAGES_PER_SEGMENT;
                    let expected_blknum = pageno % pg_constants::SLRU_PAGES_PER_SEGMENT;
                    assert!(
                        segno == expected_segno,
                        "MultiXactMembersCreate record for offset {} with unexpected key {}",
                        moff,
                        key
                    );
                    assert!(
                        blknum == expected_blknum,
                        "MultiXactMembersCreate record for offset {} with unexpected key {}",
                        moff,
                        key
                    );

                    let mut flagsval = LittleEndian::read_u32(&page[flagsoff..flagsoff + 4]);
                    flagsval &= !(((1 << pg_constants::MXACT_MEMBER_BITS_PER_XACT) - 1) << bshift);
                    flagsval |= member.status << bshift;
                    LittleEndian::write_u32(&mut page[flagsoff..flagsoff + 4], flagsval);
                    LittleEndian::write_u32(&mut page[memberoff..memberoff + 4], member.xid);
                }
            }
        }

        Ok(())
    }
}

///
/// Command with ability not to give all file descriptors to child process
///
trait CloseFileDescriptors: CommandExt {
    ///
    /// Close file descriptors (other than stdin, stdout, stderr) in child process
    ///
    fn close_fds(&mut self) -> &mut Command;
}

impl<C: CommandExt> CloseFileDescriptors for C {
    fn close_fds(&mut self) -> &mut Command {
        unsafe {
            self.pre_exec(move || {
                // SAFETY: Code executed inside pre_exec should have async-signal-safety,
                // which means it should be safe to execute inside a signal handler.
                // The precise meaning depends on platform. See `man signal-safety`
                // for the linux definition.
                //
                // The set_fds_cloexec_threadsafe function is documented to be
                // async-signal-safe.
                //
                // Aside from this function, the rest of the code is re-entrant and
                // doesn't make any syscalls. We're just passing constants.
                //
                // NOTE: It's easy to indirectly cause a malloc or lock a mutex,
                // which is not async-signal-safe. Be careful.
                close_fds::set_fds_cloexec_threadsafe(3, &[]);
                Ok(())
            })
        }
    }
}

///
/// Handle to the Postgres WAL redo process
///
struct PostgresRedoProcess {
    tenant_id: TenantId,
    child: NoLeakChild,
    stdin: ChildStdin,
    stdout: ChildStdout,
    stderr: ChildStderr,
}

impl PostgresRedoProcess {
    //
    // Start postgres binary in special WAL redo mode.
    //
    #[instrument(skip_all,fields(tenant_id=%tenant_id, pg_version=pg_version))]
    fn launch(
        conf: &PageServerConf,
        tenant_id: TenantId,
        pg_version: u32,
    ) -> Result<PostgresRedoProcess, Error> {
        // FIXME: We need a dummy Postgres cluster to run the process in. Currently, we
        // just create one with constant name. That fails if you try to launch more than
        // one WAL redo manager concurrently.
        let datadir = path_with_suffix_extension(
            conf.tenant_path(&tenant_id).join("wal-redo-datadir"),
            TEMP_FILE_SUFFIX,
        );

        // Create empty data directory for wal-redo postgres, deleting old one first.
        if datadir.exists() {
            info!(
                "old temporary datadir {} exists, removing",
                datadir.display()
            );
            fs::remove_dir_all(&datadir)?;
        }
        let pg_bin_dir_path = conf.pg_bin_dir(pg_version).map_err(|e| {
            Error::new(
                ErrorKind::Other,
                format!("incorrect pg_bin_dir path: {}", e),
            )
        })?;
        let pg_lib_dir_path = conf.pg_lib_dir(pg_version).map_err(|e| {
            Error::new(
                ErrorKind::Other,
                format!("incorrect pg_lib_dir path: {}", e),
            )
        })?;

        info!("running initdb in {}", datadir.display());
        let initdb = Command::new(pg_bin_dir_path.join("initdb"))
            .args(&["-D", &datadir.to_string_lossy()])
            .arg("-N")
            .env_clear()
            .env("LD_LIBRARY_PATH", &pg_lib_dir_path)
            .env("DYLD_LIBRARY_PATH", &pg_lib_dir_path) // macOS
            .close_fds()
            .output()
            .map_err(|e| Error::new(e.kind(), format!("failed to execute initdb: {e}")))?;

        if !initdb.status.success() {
            return Err(Error::new(
                ErrorKind::Other,
                format!(
                    "initdb failed\nstdout: {}\nstderr:\n{}",
                    String::from_utf8_lossy(&initdb.stdout),
                    String::from_utf8_lossy(&initdb.stderr)
                ),
            ));
        } else {
            // Limit shared cache for wal-redo-postgres
            let mut config = OpenOptions::new()
                .append(true)
                .open(PathBuf::from(&datadir).join("postgresql.conf"))?;
            config.write_all(b"shared_buffers=128kB\n")?;
            config.write_all(b"fsync=off\n")?;
        }

        // Start postgres itself
        let child = Command::new(pg_bin_dir_path.join("postgres"))
            .arg("--wal-redo")
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .stdout(Stdio::piped())
            .env_clear()
            .env("LD_LIBRARY_PATH", &pg_lib_dir_path)
            .env("DYLD_LIBRARY_PATH", &pg_lib_dir_path)
            .env("PGDATA", &datadir)
            // The redo process is not trusted, and runs in seccomp mode that
            // doesn't allow it to open any files. We have to also make sure it
            // doesn't inherit any file descriptors from the pageserver, that
            // would allow an attacker to read any files that happen to be open
            // in the pageserver.
            //
            // The Rust standard library makes sure to mark any file descriptors with
            // as close-on-exec by default, but that's not enough, since we use
            // libraries that directly call libc open without setting that flag.
            .close_fds()
            .spawn_no_leak_child()
            .map_err(|e| {
                Error::new(
                    e.kind(),
                    format!("postgres --wal-redo command failed to start: {}", e),
                )
            })?;

        let mut child = scopeguard::guard(child, |child| {
            error!("killing wal-redo-postgres process due to a problem during launch");
            child.kill_and_wait();
        });

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        macro_rules! set_nonblock_or_log_err {
            ($file:ident) => {{
                let res = set_nonblock($file.as_raw_fd());
                if let Err(e) = &res {
                    error!(error = %e, file = stringify!($file), pid = child.id(), "set_nonblock failed");
                }
                res
            }};
        }
        set_nonblock_or_log_err!(stdin)?;
        set_nonblock_or_log_err!(stdout)?;
        set_nonblock_or_log_err!(stderr)?;

        // all fallible operations post-spawn are complete, so get rid of the guard
        let child = scopeguard::ScopeGuard::into_inner(child);

        Ok(PostgresRedoProcess {
            tenant_id,
            child,
            stdin,
            stdout,
            stderr,
        })
    }

    #[instrument(skip_all, fields(tenant_id=%self.tenant_id, pid=%self.child.id()))]
    fn kill(self) {
        self.child.kill_and_wait();
    }

    //
    // Apply given WAL records ('records') over an old page image. Returns
    // new page image.
    //
    #[instrument(skip_all, fields(tenant_id=%self.tenant_id, pid=%self.child.id()))]
    fn apply_wal_records(
        &mut self,
        tag: BufferTag,
        base_img: Option<Bytes>,
        records: &[(Lsn, NeonWalRecord)],
        wal_redo_timeout: Duration,
    ) -> Result<Bytes, std::io::Error> {
        // Serialize all the messages to send the WAL redo process first.
        //
        // This could be problematic if there are millions of records to replay,
        // but in practice the number of records is usually so small that it doesn't
        // matter, and it's better to keep this code simple.
        //
        // Most requests start with a before-image with BLCKSZ bytes, followed by
        // by some other WAL records. Start with a buffer that can hold that
        // comfortably.
        let mut writebuf: Vec<u8> = Vec::with_capacity((BLCKSZ as usize) * 3);
        build_begin_redo_for_block_msg(tag, &mut writebuf);
        if let Some(img) = base_img {
            build_push_page_msg(tag, &img, &mut writebuf);
        }
        for (lsn, rec) in records.iter() {
            if let NeonWalRecord::Postgres {
                will_init: _,
                rec: postgres_rec,
            } = rec
            {
                build_apply_record_msg(*lsn, postgres_rec, &mut writebuf);
            } else {
                return Err(Error::new(
                    ErrorKind::Other,
                    "tried to pass neon wal record to postgres WAL redo",
                ));
            }
        }
        build_get_page_msg(tag, &mut writebuf);
        WAL_REDO_RECORD_COUNTER.inc_by(records.len() as u64);

        // The input is now in 'writebuf'. Do a blind write first, writing as much as
        // we can, before calling poll(). That skips one call to poll() if the stdin is
        // already available for writing, which it almost certainly is because the
        // process is idle.
        let mut nwrite = self.stdin.write(&writebuf)?;

        // We expect the WAL redo process to respond with an 8k page image. We read it
        // into this buffer.
        let mut resultbuf = vec![0; BLCKSZ.into()];
        let mut nresult: usize = 0; // # of bytes read into 'resultbuf' so far

        // Prepare for calling poll()
        let mut pollfds = [
            PollFd::new(self.stdout.as_raw_fd(), PollFlags::POLLIN),
            PollFd::new(self.stderr.as_raw_fd(), PollFlags::POLLIN),
            PollFd::new(self.stdin.as_raw_fd(), PollFlags::POLLOUT),
        ];

        // We do three things simultaneously: send the old base image and WAL records to
        // the child process's stdin, read the result from child's stdout, and forward any logging
        // information that the child writes to its stderr to the page server's log.
        while nresult < BLCKSZ.into() {
            // If we have more data to write, wake up if 'stdin' becomes writeable or
            // we have data to read. Otherwise only wake up if there's data to read.
            let nfds = if nwrite < writebuf.len() { 3 } else { 2 };
            let n = loop {
                match nix::poll::poll(&mut pollfds[0..nfds], wal_redo_timeout.as_millis() as i32) {
                    Err(e) if e == nix::errno::Errno::EINTR => continue,
                    res => break res,
                }
            }?;

            if n == 0 {
                return Err(Error::new(ErrorKind::Other, "WAL redo timed out"));
            }

            // If we have some messages in stderr, forward them to the log.
            let err_revents = pollfds[1].revents().unwrap();
            if err_revents & (PollFlags::POLLERR | PollFlags::POLLIN) != PollFlags::empty() {
                let mut errbuf: [u8; 16384] = [0; 16384];
                let n = self.stderr.read(&mut errbuf)?;

                // The message might not be split correctly into lines here. But this is
                // good enough, the important thing is to get the message to the log.
                if n > 0 {
                    error!(
                        "wal-redo-postgres: {}",
                        String::from_utf8_lossy(&errbuf[0..n])
                    );

                    // To make sure we capture all log from the process if it fails, keep
                    // reading from the stderr, before checking the stdout.
                    continue;
                }
            } else if err_revents.contains(PollFlags::POLLHUP) {
                return Err(Error::new(
                    ErrorKind::BrokenPipe,
                    "WAL redo process closed its stderr unexpectedly",
                ));
            }

            // If we have more data to write and 'stdin' is writeable, do write.
            if nwrite < writebuf.len() {
                let in_revents = pollfds[2].revents().unwrap();
                if in_revents & (PollFlags::POLLERR | PollFlags::POLLOUT) != PollFlags::empty() {
                    nwrite += self.stdin.write(&writebuf[nwrite..])?;
                } else if in_revents.contains(PollFlags::POLLHUP) {
                    // We still have more data to write, but the process closed the pipe.
                    return Err(Error::new(
                        ErrorKind::BrokenPipe,
                        "WAL redo process closed its stdin unexpectedly",
                    ));
                }
            }

            // If we have some data in stdout, read it to the result buffer.
            let out_revents = pollfds[0].revents().unwrap();
            if out_revents & (PollFlags::POLLERR | PollFlags::POLLIN) != PollFlags::empty() {
                nresult += self.stdout.read(&mut resultbuf[nresult..])?;
            } else if out_revents.contains(PollFlags::POLLHUP) {
                return Err(Error::new(
                    ErrorKind::BrokenPipe,
                    "WAL redo process closed its stdout unexpectedly",
                ));
            }
        }

        Ok(Bytes::from(resultbuf))
    }
}

/// Wrapper type around `std::process::Child` which guarantees that the child
/// will be killed and waited-for by this process before being dropped.
struct NoLeakChild {
    child: Option<Child>,
}

impl Deref for NoLeakChild {
    type Target = Child;

    fn deref(&self) -> &Self::Target {
        self.child.as_ref().expect("must not use from drop")
    }
}

impl DerefMut for NoLeakChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.child.as_mut().expect("must not use from drop")
    }
}

impl NoLeakChild {
    fn spawn(command: &mut Command) -> io::Result<Self> {
        let child = command.spawn()?;
        Ok(NoLeakChild { child: Some(child) })
    }

    fn kill_and_wait(mut self) {
        let child = match self.child.take() {
            Some(child) => child,
            None => return,
        };
        Self::kill_and_wait_impl(child);
    }

    #[instrument(skip_all, fields(pid=child.id()))]
    fn kill_and_wait_impl(mut child: Child) {
        let res = child.kill();
        if let Err(e) = res {
            // This branch is very unlikely because:
            // - We (= pageserver) spawned this process successfully, so, we're allowed to kill it.
            // - This is the only place that calls .kill()
            // - We consume `self`, so, .kill() can't be called twice.
            // - If the process exited by itself or was killed by someone else,
            //   .kill() will still succeed because we haven't wait()'ed yet.
            //
            // So, if we arrive here, we have really no idea what happened,
            // whether the PID stored in self.child is still valid, etc.
            // If this function were fallible, we'd return an error, but
            // since it isn't, all we can do is log an error and proceed
            // with the wait().
            error!(error = %e, "failed to SIGKILL; subsequent wait() might fail or wait for wrong process");
        }

        match child.wait() {
            Ok(exit_status) => {
                info!(exit_status = %exit_status, "wait successful");
            }
            Err(e) => {
                error!(error = %e, "wait error; might leak the child process; it will show as zombie (defunct)");
            }
        }
    }
}

impl Drop for NoLeakChild {
    fn drop(&mut self) {
        let child = match self.child.take() {
            Some(child) => child,
            None => return,
        };
        // Offload the kill+wait of the child process into the background.
        // If someone stops the runtime, we'll leak the child process.
        // We can ignore that case because we only stop the runtime on pageserver exit.
        BACKGROUND_RUNTIME.spawn(async move {
            tokio::task::spawn_blocking(move || {
                Self::kill_and_wait_impl(child);
            })
            .await
        });
    }
}

trait NoLeakChildCommandExt {
    fn spawn_no_leak_child(&mut self) -> io::Result<NoLeakChild>;
}

impl NoLeakChildCommandExt for Command {
    fn spawn_no_leak_child(&mut self) -> io::Result<NoLeakChild> {
        NoLeakChild::spawn(self)
    }
}

// Functions for constructing messages to send to the postgres WAL redo
// process. See pgxn/neon_walredo/walredoproc.c for
// explanation of the protocol.

fn build_begin_redo_for_block_msg(tag: BufferTag, buf: &mut Vec<u8>) {
    let len = 4 + 1 + 4 * 4;

    buf.put_u8(b'B');
    buf.put_u32(len as u32);

    tag.ser_into(buf)
        .expect("serialize BufferTag should always succeed");
}

fn build_push_page_msg(tag: BufferTag, base_img: &[u8], buf: &mut Vec<u8>) {
    assert!(base_img.len() == 8192);

    let len = 4 + 1 + 4 * 4 + base_img.len();

    buf.put_u8(b'P');
    buf.put_u32(len as u32);
    tag.ser_into(buf)
        .expect("serialize BufferTag should always succeed");
    buf.put(base_img);
}

fn build_apply_record_msg(endlsn: Lsn, rec: &[u8], buf: &mut Vec<u8>) {
    let len = 4 + 8 + rec.len();

    buf.put_u8(b'A');
    buf.put_u32(len as u32);
    buf.put_u64(endlsn.0);
    buf.put(rec);
}

fn build_get_page_msg(tag: BufferTag, buf: &mut Vec<u8>) {
    let len = 4 + 1 + 4 * 4;

    buf.put_u8(b'G');
    buf.put_u32(len as u32);
    tag.ser_into(buf)
        .expect("serialize BufferTag should always succeed");
}
