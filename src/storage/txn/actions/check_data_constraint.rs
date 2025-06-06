// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.

// #[PerformanceCriticalPath]
use txn_types::{Key, TimeStamp, Write, WriteType};

use crate::storage::{
    Snapshot,
    mvcc::{ErrorInner, Result as MvccResult, SnapshotReader},
};

/// Checks the existence of the key according to `should_not_exist`.
/// If not, returns an `AlreadyExist` error.
/// The caller must guarantee that the given `write` is the latest version of
/// the key.
pub(crate) fn check_data_constraint<S: Snapshot>(
    reader: &mut SnapshotReader<S>,
    should_not_exist: bool,
    write: &Write,
    write_commit_ts: TimeStamp,
    key: &Key,
) -> MvccResult<()> {
    // Here we assume `write` is the latest version of the key. So it should not
    // contain a GC fence ts. Otherwise, it must be an already-deleted version.
    let write_is_invalid = matches!(write.gc_fence, Some(gc_fence_ts) if !gc_fence_ts.is_zero());

    if !should_not_exist || write.write_type == WriteType::Delete || write_is_invalid {
        return Ok(());
    }

    // The current key exists under any of the following conditions:
    // 1.The current write type is `PUT`
    // 2.The current write type is `Rollback` or `Lock`, and the key have an older
    // version.
    let existing_start_ts = if write.write_type == WriteType::Put {
        Some(write.start_ts)
    } else if let Some(prev_write) = reader.get_write(key, write_commit_ts.prev())? {
        Some(prev_write.start_ts)
    } else {
        None
    };

    if let Some(existing_start_ts) = existing_start_ts {
        return Err(ErrorInner::AlreadyExist {
            key: key.to_raw()?,
            existing_start_ts,
        }
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use concurrency_manager::ConcurrencyManager;
    use kvproto::kvrpcpb::Context;

    use super::*;
    use crate::storage::{
        Engine, TestEngineBuilder,
        mvcc::{MvccTxn, tests::write},
    };

    #[test]
    fn test_check_data_constraint() {
        let mut engine = TestEngineBuilder::new().build().unwrap();
        let cm = ConcurrencyManager::new_for_test(42.into());
        let mut txn = MvccTxn::new(TimeStamp::new(2), cm);
        txn.put_write(
            Key::from_raw(b"a"),
            TimeStamp::new(5),
            Write::new(WriteType::Put, TimeStamp::new(2), None)
                .as_ref()
                .to_bytes(),
        );
        write(&engine, &Context::default(), txn.into_modifies());
        let snapshot = engine.snapshot(Default::default()).unwrap();
        let mut reader = SnapshotReader::new(TimeStamp::new(3), snapshot, true);

        struct Case {
            expected: MvccResult<()>,

            should_not_exist: bool,
            write: Write,
            write_commit_ts: TimeStamp,
            key: Key,
        }

        let cases = vec![
            // todo: add more cases
            Case {
                // should skip the check when `should_not_exist` is `false`
                expected: Ok(()),
                should_not_exist: false,
                write: Write::new(WriteType::Put, TimeStamp::new(3), None),
                write_commit_ts: Default::default(),
                key: Key::from_raw(b"a"),
            },
            Case {
                // should skip the check when `write_type` is `delete`
                expected: Ok(()),
                should_not_exist: true,
                write: Write::new(WriteType::Delete, TimeStamp::new(3), None),
                write_commit_ts: Default::default(),
                key: Key::from_raw(b"a"),
            },
            Case {
                // should detect conflict `Put`
                expected: Err(ErrorInner::AlreadyExist {
                    key: b"a".to_vec(),
                    existing_start_ts: TimeStamp::new(3),
                }
                .into()),
                should_not_exist: true,
                write: Write::new(WriteType::Put, TimeStamp::new(3), None),
                write_commit_ts: Default::default(),
                key: Key::from_raw(b"a"),
            },
            Case {
                // should detect an older version when the current write type is `Rollback`
                expected: Err(ErrorInner::AlreadyExist {
                    key: b"a".to_vec(),
                    existing_start_ts: TimeStamp::new(2),
                }
                .into()),
                should_not_exist: true,
                write: Write::new(WriteType::Rollback, TimeStamp::new(3), None),
                write_commit_ts: TimeStamp::new(6),
                key: Key::from_raw(b"a"),
            },
            Case {
                // should detect an older version when the current write type is `Lock`
                expected: Err(ErrorInner::AlreadyExist {
                    key: b"a".to_vec(),
                    existing_start_ts: TimeStamp::new(2),
                }
                .into()),
                should_not_exist: true,
                write: Write::new(WriteType::Lock, TimeStamp::new(10), None),
                write_commit_ts: TimeStamp::new(15),
                key: Key::from_raw(b"a"),
            },
        ];

        for Case {
            expected,
            should_not_exist,
            write,
            write_commit_ts,
            key,
        } in cases
        {
            let result =
                check_data_constraint(&mut reader, should_not_exist, &write, write_commit_ts, &key);
            assert_eq!(format!("{:?}", expected), format!("{:?}", result));
        }
    }
}
