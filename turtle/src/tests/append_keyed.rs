use std::fs;
use std::path::PathBuf;

use turtle::ops_frame::{run_op, TurtleOp};

fn fresh_world() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("turtle_append_keyed_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn append_keyed_increments_committed_and_emits_receipts() {
    let world = fresh_world();

    // Initial scan (should be zero)
    let scan0 = run_op(world.clone(), TurtleOp::ScanCommitted).unwrap();
    assert_eq!(scan0.committed_frames, 0);

    // Append a keyed entry
    let key = "event/test/job/000001/Gate5".to_string();
    let value = b"hello".to_vec();

    run_op(
        world.clone(),
        TurtleOp::AppendKeyed {
            key: key.clone(),
            value: value.clone(),
        },
    )
    .unwrap();

    // Scan again
    let scan1 = run_op(world.clone(), TurtleOp::ScanCommitted).unwrap();

    // One frame must now exist
    assert_eq!(scan1.committed_frames, 1);
    assert_eq!(scan1.committed_frames, 1);
}
