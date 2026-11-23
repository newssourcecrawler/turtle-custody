use std::fs;
use std::path::PathBuf;

use turtle::ops_frame::{run_op, TurtleOp};

fn fresh_world() -> PathBuf {
    // Deterministic, per-process temp dir (good enough for our test runner).
    let dir = std::env::temp_dir().join(format!("turtle_readtail_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn readtail_is_read_only_and_succeeds() {
    let world = fresh_world();

    // Append 5 frames
    for i in 0..5u8 {
        let payload = vec![i; 16];
        run_op(world.clone(), TurtleOp::AppendBytes { payload }).unwrap();
    }

    let scan0 = run_op(world.clone(), TurtleOp::ScanCommitted).unwrap();
    assert_eq!(scan0.committed_frames, 5);

    // Read tail bounded to 3 frames (v0: emits receipts; we only assert it succeeds).
    run_op(
        world.clone(),
        TurtleOp::ReadTail {
            max_frames: 3,
            max_bytes: 10_000,
        },
    )
    .unwrap();

    // Bounds edge cases
    run_op(
        world.clone(),
        TurtleOp::ReadTail {
            max_frames: 0,
            max_bytes: 0,
        },
    )
    .unwrap();

    // Ensure ReadTail is read-only: committed state unchanged.
    let scan1 = run_op(world.clone(), TurtleOp::ScanCommitted).unwrap();
    assert_eq!(scan1.committed_frames, 5);
}
