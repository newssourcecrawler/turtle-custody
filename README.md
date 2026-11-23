# Turtle

Turtle is an append-only Rust ledger for committing and retrieving application
state on a local filesystem. Version `0.0.2-dev.1` is an independent experimental
candidate being prepared for possible submission to the CNCF Sandbox.

## Implemented custody

- `init` creates a working directory and a 256 MiB preallocated ledger.
  `append` stores an opaque payload in a checksummed frame and acknowledges its
  commit sequence number (CSN) after syncing the frame and its commit marker.
  The CLI writes BLAKE3 checksums by default.
- `verify` scans committed frames and reports the observed CSN. `tail` retrieves
  the newest committed payloads. `status` and `doctor` expose local diagnostics.
- The CLI provides an uncommitted staging area with `mush-put`, `mush-get`, and
  `mush-ls`. `promote` commits a staged object. The `ds-*` commands stage,
  commit, and retrieve text records with caller-supplied dataset metadata.
- The Rust library exposes byte-payload appends, keyed appends, committed
  scans, memory-mapped readback, and bounded payload-range reads by block
  location.

## Build and test offline

Use Rust and Cargo with the host linker and C build tools. The locked offline
build and test run for this candidate used Rust and Cargo 1.99.0 on macOS ARM64.
The `nsc_block` and `nsc_frame` sources are vendored. Cargo needs the remaining
registry dependencies in its local cache before an offline build.

Run from the repository root.

```sh
cargo build --locked --offline --workspace --bin turtle
cargo test --locked --offline --workspace
```

The retained suite ran **22 tests**, covering partial writes, checksum failures,
capacity limits, keyed appends, bounded tail reads, and payload-range reads.

## Commit, verify, and retrieve a payload

Run this from the same directory after building. The example uses a fresh
working directory. It checks both the acknowledgment and the verifier's CSN,
then compares the retrieved payload byte for byte.

```sh
(
    set -eu

    turtle="${CARGO_TARGET_DIR:-target}/debug/turtle"
    world=$(mktemp -d "${TMPDIR:-/tmp}/turtle-quickstart.XXXXXX")
    payload='{"state":{"position":1,"value":42}}'

    "$turtle" init --dir "$world"
    ack=$("$turtle" append --dir "$world" --record "$payload")
    printf '%s\n' "$ack"
    test "$ack" = 'ack csn=1'

    verified=$("$turtle" verify --dir "$world")
    printf '%s\n' "$verified"
    case "$verified" in
        'committed_csn=1 '*) ;;
        *) printf '%s\n' 'Expected committed CSN 1' >&2; exit 1 ;;
    esac

    "$turtle" tail --dir "$world" --n 1 > "$world/retrieved.json"
    printf '%s' "$payload" > "$world/expected.json"
    cmp "$world/expected.json" "$world/retrieved.json"
    printf 'Verified the exact payload at CSN 1 in %s\n' "$world"
)
```

The recorded smoke test used the same payload and verified CSN 1. The ledger
hash stayed unchanged during verification and readback. `verify` can refresh
the derived status file.

## Limits and roadmap

- Plan for one writer per ledger. The write CLI uses a local advisory directory
  lock for `append`, `promote`, and `ds-close`. Direct library callers must
  serialize writers themselves. Distributed coordination, stale-writer fencing,
  and automatic reassignment are outside this baseline.
- The scanner reports the valid committed history. It stops at incomplete or
  invalid frames and performs no repair. Keep an expected CSN and payload or
  digest to detect missing history.
- Applications supply and interpret the state bytes and define how work resumes.
  This baseline provides neither workload scheduling nor automatic recovery of
  external side effects. The dataset commands handle local text records.
  Native z/OS dataset access and IBM037/COMP-3 decoding are outside their scope.
- The retained evidence covers local custody on macOS ARM64. A z/OS-to-Linux
  handoff is a future objective. It would require a source adapter to commit a
  workload checkpoint, verified transfer of that checkpoint and its required
  inputs, and a destination adapter to resume from the saved position. Broader
  OS-to-OS continuation is a longer-term objective.

## License

The workspace declares `MIT OR Apache-2.0`. See [LICENSE-APACHE](LICENSE-APACHE).
Vendored crates retain their own license declarations and texts.
