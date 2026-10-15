//! STO-20: model-based property tests of the CoW B+tree against `std::collections::BTreeMap`.
//!
//! Lease STO-20 writes this file. Required tests (proptest 1.12, `ProptestConfig::with_cases(64)`
//! default, deterministic seeds via `PROPTEST_RNG_SEED` honoured by proptest):
//! 1. `random_ops_match_btreemap`: over a `MemVfs` container (page_size 4096 to force splits),
//!    apply 1..2000 random put/delete ops (keys 0..=40 bytes from a small alphabet so collisions
//!    happen; values 0..=6000 bytes so overflow chains occur) inside ONE WriteTxn, then commit;
//!    after every 100 ops and at the end compare full `scan(Unbounded, Unbounded)` and random
//!    `get`s with the model. Then reopen and compare again.
//! 2. `snapshots_are_immutable`: take `TreeSnapshot`s after each of several commits; later
//!    commits (including deletes of everything) never change what old snapshots return.
//! 3. `range_bounds`: every combination of Included/Excluded/Unbounded bounds equals
//!    `BTreeMap::range` (and empty/inverted ranges yield nothing without panicking).
//! 4. `sequential_append_fill`: inserting 100k ascending 16-byte keys yields leaves that are >= 90%
//!    full on average (append split heuristic), checked through `space_report`.
//! 5. `delete_all_returns_null_root`: deleting every key returns `PageId::NONE`.
//! 6. `doctor_clean`: `Container::verify(.., VerifyLevel::Full)` reports no Error issues after
//!    each scenario above.

use pgarbor_common::{KvRead, PageId};
use pgarbor_storage::{
    Container, CreateOptions, IssueSeverity, MemVfs, OpenOptions, PrefixU64Roots, SyncMode, TreeSnapshot, VerifyLevel,
    Vfs, WriteTxn,
};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;

type Model = BTreeMap<Vec<u8>, Vec<u8>>;
type Pairs = Vec<(Vec<u8>, Vec<u8>)>;

const DB_PATH: &str = "model.pga";
const PAGE_SIZE: u32 = 4096;
const ROOT_KEY: &[u8] = b"Rmain";

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Op {
    Put(Vec<u8>, Vec<u8>),
    Del(Vec<u8>),
}

fn create_opts() -> CreateOptions {
    CreateOptions { page_size: PAGE_SIZE, sync: SyncMode::Off, cache_pages: 512, ..CreateOptions::default() }
}

fn fresh_with_page_size(page_size: u32) -> (MemVfs, Container, CreateOptions) {
    let vfs = MemVfs::new();
    let opts = CreateOptions { page_size, ..create_opts() };
    let shared: Arc<dyn Vfs> = Arc::new(vfs.clone());
    let c = Container::create_with(shared, Path::new(DB_PATH), &opts).expect("create container over MemVfs");
    (vfs, c, opts)
}

fn fresh() -> (MemVfs, Container, CreateOptions) {
    fresh_with_page_size(PAGE_SIZE)
}

fn reopen(vfs: &MemVfs, opts: &CreateOptions) -> Container {
    let shared: Arc<dyn Vfs> = Arc::new(vfs.clone());
    Container::open_with(shared, Path::new(DB_PATH), &OpenOptions::from_create(opts)).expect("reopen container")
}

/// Small alphabet, mostly short keys so that collisions (overwrites/deletes of existing keys) happen.
fn key_strategy() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        3 => prop::collection::vec(97u8..100, 0..=4),
        1 => prop::collection::vec(97u8..101, 0..=40),
    ]
}

/// 0..=6000 bytes; the larger sizes exceed `max_inline_value(4096) == 1024` and use overflow chains.
fn value_strategy() -> impl Strategy<Value = Vec<u8>> {
    (prop_oneof![6 => 0usize..=64, 2 => 65usize..=1500, 1 => 1501usize..=6000], any::<u8>()).prop_map(|(len, seed)| {
        (0..len).map(|i| seed.wrapping_add((i % 251) as u8)).collect::<Vec<u8>>()
    })
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (key_strategy(), value_strategy()).prop_map(|(k, v)| Op::Put(k, v)),
        1 => key_strategy().prop_map(Op::Del),
    ]
}

fn collect_iter<I>(it: I) -> Pairs
where
    I: Iterator<Item = pgarbor_common::Result<(Vec<u8>, Vec<u8>)>>,
{
    it.map(|r| r.expect("scan item")).collect()
}

fn model_pairs(model: &Model) -> Pairs {
    model.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

fn bound_ref(b: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match b {
        Bound::Included(k) => Bound::Included(k.as_slice()),
        Bound::Excluded(k) => Bound::Excluded(k.as_slice()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

fn make_bound(kind: u8, key: &[u8]) -> Bound<Vec<u8>> {
    match kind % 3 {
        0 => Bound::Included(key.to_vec()),
        1 => Bound::Excluded(key.to_vec()),
        _ => Bound::Unbounded,
    }
}

fn in_bounds(k: &[u8], lo: &Bound<Vec<u8>>, hi: &Bound<Vec<u8>>) -> bool {
    let lo_ok = match lo {
        Bound::Included(b) => k >= b.as_slice(),
        Bound::Excluded(b) => k > b.as_slice(),
        Bound::Unbounded => true,
    };
    let hi_ok = match hi {
        Bound::Included(b) => k <= b.as_slice(),
        Bound::Excluded(b) => k < b.as_slice(),
        Bound::Unbounded => true,
    };
    lo_ok && hi_ok
}

/// `BTreeMap::range` panics on inverted ranges and on `(Excluded(x), Excluded(x))`.
fn btreemap_range_is_valid(lo: &Bound<Vec<u8>>, hi: &Bound<Vec<u8>>) -> bool {
    match (lo, hi) {
        (Bound::Unbounded, _) | (_, Bound::Unbounded) => true,
        (Bound::Included(a), Bound::Included(b)) => a <= b,
        (Bound::Included(a), Bound::Excluded(b)) | (Bound::Excluded(a), Bound::Included(b)) => a <= b,
        (Bound::Excluded(a), Bound::Excluded(b)) => a < b,
    }
}

/// Compare an actual full scan and point lookups with the model.
fn check_state(
    what: &str,
    actual: Pairs,
    get: &dyn Fn(&[u8]) -> Option<Vec<u8>>,
    model: &Model,
    probes: &[Vec<u8>],
) -> Result<(), TestCaseError> {
    let expected = model_pairs(model);
    prop_assert_eq!(actual.len(), expected.len(), "{}: entry count differs", what);
    for (a, e) in actual.iter().zip(expected.iter()) {
        prop_assert_eq!(&a.0, &e.0, "{}: key mismatch", what);
        prop_assert!(a.1 == e.1, "{}: value mismatch for key {:?} ({} vs {} bytes)", what, a.0, a.1.len(), e.1.len());
    }
    for p in probes {
        let got = get(p);
        prop_assert!(got.as_ref() == model.get(p), "{}: get({:?}) differs from model", what, p);
    }
    // A sample of present keys (every key for small models).
    let stride = (model.len() / 64).max(1);
    for (k, v) in model.iter().step_by(stride) {
        let got = get(k);
        prop_assert!(got.as_ref() == Some(v), "{}: get of present key {:?} differs", what, k);
    }
    Ok(())
}

fn check_txn(w: &WriteTxn<'_>, root: PageId, model: &Model, probes: &[Vec<u8>]) -> Result<(), TestCaseError> {
    let it = w.scan(root, Bound::Unbounded, Bound::Unbounded).expect("txn scan");
    check_state("write txn", collect_iter(it), &|k: &[u8]| w.get(root, k).expect("txn get"), model, probes)
}

fn check_tree(t: &TreeSnapshot, model: &Model, probes: &[Vec<u8>]) -> Result<(), TestCaseError> {
    let it = t.scan(Bound::Unbounded, Bound::Unbounded).expect("snapshot scan");
    check_state("snapshot", collect_iter(it), &|k: &[u8]| t.get(k).expect("snapshot get"), model, probes)
}

fn apply_op(w: &mut WriteTxn<'_>, root: &mut PageId, model: &mut Model, op: &Op) -> Result<(), TestCaseError> {
    match op {
        Op::Put(k, v) => {
            let r = w.put(*root, k, v).expect("put");
            *root = r.root;
            let prev = model.insert(k.clone(), v.clone());
            prop_assert_eq!(r.replaced, prev.is_some(), "PutResult.replaced for key {:?}", k);
        }
        Op::Del(k) => {
            let r = w.delete(*root, k).expect("delete");
            *root = r.root;
            let prev = model.remove(k);
            prop_assert_eq!(r.existed, prev.is_some(), "DeleteResult.existed for key {:?}", k);
            if model.is_empty() {
                prop_assert_eq!(*root, PageId::NONE, "empty tree must have the null root");
            }
        }
    }
    Ok(())
}

/// Store the data tree root in the directory under `Rmain` and commit.
fn publish_and_commit(mut w: WriteTxn<'_>, root: PageId) {
    let dr = w.directory_root();
    let dir = w.put(dr, ROOT_KEY, &root.0.to_le_bytes()).expect("put directory entry").root;
    w.set_directory_root(dir);
    w.commit().expect("commit");
}

fn read_published_root(c: &Container) -> PageId {
    let snap = c.snapshot();
    let v = snap.directory().get(ROOT_KEY).expect("directory get").expect("directory has Rmain");
    let b: [u8; 8] = v.as_slice().try_into().expect("8-byte root reference");
    PageId(u64::from_le_bytes(b))
}

/// Doctor must report no Error-severity issues at `VerifyLevel::Full`.
fn assert_doctor_clean(c: &Container) {
    let rep = c.verify(&PrefixU64Roots::new(b"R"), VerifyLevel::Full).expect("verify Full");
    let errs: Vec<String> = rep
        .issues
        .iter()
        .filter(|i| matches!(i.severity, IssueSeverity::Error))
        .map(|i| format!("{:?}: {}", i.page, i.message))
        .collect();
    assert!(errs.is_empty(), "doctor reported errors: {errs:#?}");
}

// ---------------------------------------------------------------------------------------------
// 1. random_ops_match_btreemap
// ---------------------------------------------------------------------------------------------

fn run_model(ops: &[Op], probes: &[Vec<u8>]) -> Result<(), TestCaseError> {
    let (vfs, c, opts) = fresh();
    let mut model = Model::new();
    let mut root = PageId::NONE;
    let mut w = c.begin_write().expect("begin_write");
    for (i, op) in ops.iter().enumerate() {
        apply_op(&mut w, &mut root, &mut model, op)?;
        if (i + 1) % 100 == 0 {
            check_txn(&w, root, &model, probes)?;
        }
    }
    check_txn(&w, root, &model, probes)?;
    publish_and_commit(w, root);

    // Committed state through a snapshot.
    {
        let snap = c.snapshot();
        let t = snap.tree(root).expect("tree of committed root");
        check_tree(&t, &model, probes)?;
    }
    assert_doctor_clean(&c);
    c.close().expect("close");

    // Reopen and compare again.
    let c2 = reopen(&vfs, &opts);
    let root2 = read_published_root(&c2);
    prop_assert_eq!(root2, root, "published root survives reopen");
    {
        let snap = c2.snapshot();
        let t = snap.tree(root2).expect("tree after reopen");
        check_tree(&t, &model, probes)?;
    }
    assert_doctor_clean(&c2);
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn random_ops_match_btreemap(
        ops in prop::collection::vec(op_strategy(), 1..2000),
        probes in prop::collection::vec(key_strategy(), 0..32),
    ) {
        run_model(&ops, &probes)?;
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    #[ignore = "heavy: 512 cases of up to 8000 ops"]
    fn random_ops_match_btreemap_heavy(
        ops in prop::collection::vec(op_strategy(), 1..8000),
        probes in prop::collection::vec(key_strategy(), 0..64),
    ) {
        run_model(&ops, &probes)?;
    }
}

// ---------------------------------------------------------------------------------------------
// 2. snapshots_are_immutable
// ---------------------------------------------------------------------------------------------

fn run_snapshots(groups: &[Vec<Op>], probes: &[Vec<u8>]) -> Result<(), TestCaseError> {
    let (_vfs, c, _opts) = fresh();
    let mut model = Model::new();
    let mut root = PageId::NONE;
    let mut snaps: Vec<(TreeSnapshot, Model)> = Vec::new();

    for group in groups {
        let mut w = c.begin_write().expect("begin_write");
        for op in group {
            apply_op(&mut w, &mut root, &mut model, op)?;
        }
        publish_and_commit(w, root);
        let snap = c.snapshot();
        snaps.push((snap.tree(root).expect("tree after commit"), model.clone()));
        for (t, m) in &snaps {
            check_tree(t, m, probes)?;
        }
    }

    // Delete everything in a final commit.
    let keys: Vec<Vec<u8>> = model.keys().cloned().collect();
    let mut w = c.begin_write().expect("begin_write");
    for k in keys {
        apply_op(&mut w, &mut root, &mut model, &Op::Del(k))?;
    }
    prop_assert!(model.is_empty());
    prop_assert_eq!(root, PageId::NONE);
    publish_and_commit(w, root);
    let snap = c.snapshot();
    snaps.push((snap.tree(root).expect("empty tree"), model.clone()));
    for (t, m) in &snaps {
        check_tree(t, m, probes)?;
    }
    assert_doctor_clean(&c);
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn snapshots_are_immutable(
        groups in prop::collection::vec(prop::collection::vec(op_strategy(), 0..120), 1..7),
        probes in prop::collection::vec(key_strategy(), 0..16),
    ) {
        run_snapshots(&groups, &probes)?;
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    #[ignore = "heavy: 256 cases of up to 20 commit groups"]
    fn snapshots_are_immutable_heavy(
        groups in prop::collection::vec(prop::collection::vec(op_strategy(), 0..400), 1..20),
        probes in prop::collection::vec(key_strategy(), 0..32),
    ) {
        run_snapshots(&groups, &probes)?;
    }
}

// ---------------------------------------------------------------------------------------------
// 3. range_bounds
// ---------------------------------------------------------------------------------------------

fn run_ranges(entries: &[(Vec<u8>, Vec<u8>)], lo_key: &[u8], hi_key: &[u8]) -> Result<(), TestCaseError> {
    let (_vfs, c, _opts) = fresh();
    let mut model = Model::new();
    let mut root = PageId::NONE;
    let mut w = c.begin_write().expect("begin_write");
    for (k, v) in entries {
        apply_op(&mut w, &mut root, &mut model, &Op::Put(k.clone(), v.clone()))?;
    }

    // Every combination of bound kinds, in both key orders (the second is an inverted range
    // whenever lo_key < hi_key), plus equal-key ranges.
    let mut cases: Vec<(Bound<Vec<u8>>, Bound<Vec<u8>>)> = Vec::new();
    for (a, b) in [(lo_key, hi_key), (hi_key, lo_key), (lo_key, lo_key)] {
        for lk in 0..3u8 {
            for hk in 0..3u8 {
                cases.push((make_bound(lk, a), make_bound(hk, b)));
            }
        }
    }

    let check_one = |actual: Pairs, lo: &Bound<Vec<u8>>, hi: &Bound<Vec<u8>>, what: &str| -> Result<(), TestCaseError> {
        let expected: Pairs =
            model.iter().filter(|(k, _)| in_bounds(k, lo, hi)).map(|(k, v)| (k.clone(), v.clone())).collect();
        prop_assert!(actual == expected, "{}: range {:?}..{:?} differs from model", what, lo, hi);
        if btreemap_range_is_valid(lo, hi) {
            let via_std: Pairs = model
                .range::<[u8], _>((bound_ref(lo), bound_ref(hi)))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            prop_assert!(actual == via_std, "{}: range {:?}..{:?} differs from BTreeMap::range", what, lo, hi);
        }
        Ok(())
    };

    for (lo, hi) in &cases {
        let it = w
            .scan(root, bound_ref(lo), bound_ref(hi))
            .expect("txn range scan (must not fail on empty or inverted ranges)");
        check_one(collect_iter(it), lo, hi, "write txn")?;
    }

    publish_and_commit(w, root);
    let snap = c.snapshot();
    let t = snap.tree(root).expect("tree");
    for (lo, hi) in &cases {
        let it = t.scan(bound_ref(lo), bound_ref(hi)).expect("snapshot range scan");
        check_one(collect_iter(it), lo, hi, "TreeSnapshot::scan")?;
        let it = t.range(bound_ref(lo), bound_ref(hi)).expect("snapshot range");
        check_one(collect_iter(it), lo, hi, "TreeSnapshot::range")?;
    }
    drop(t);
    drop(snap);
    assert_doctor_clean(&c);
    Ok(())
}

fn range_entries(max: usize) -> impl Strategy<Value = Vec<(Vec<u8>, Vec<u8>)>> {
    prop::collection::vec((prop::collection::vec(97u8..100, 0..=4), prop::collection::vec(any::<u8>(), 0..=40)), 0..max)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn range_bounds(
        entries in range_entries(150),
        lo_key in prop::collection::vec(97u8..100, 0..=4),
        hi_key in prop::collection::vec(97u8..100, 0..=4),
    ) {
        run_ranges(&entries, &lo_key, &hi_key)?;
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    #[ignore = "heavy: 512 cases with large trees"]
    fn range_bounds_heavy(
        entries in range_entries(1500),
        lo_key in prop::collection::vec(97u8..100, 0..=4),
        hi_key in prop::collection::vec(97u8..100, 0..=4),
    ) {
        run_ranges(&entries, &lo_key, &hi_key)?;
    }
}

/// Fixed inputs: empty tree and tiny tree with degenerate ranges never panic or error.
#[test]
fn range_degenerate_cases() {
    const A: &[u8] = b"a";
    const B: &[u8] = b"b";
    const C: &[u8] = b"c";
    let (_vfs, c, _opts) = fresh();
    let mut w = c.begin_write().expect("begin_write");
    // Empty tree (null root) with an inverted range.
    let it = w
        .scan(PageId::NONE, Bound::Included(B), Bound::Excluded(A))
        .expect("scan of the null root with an inverted range");
    assert_eq!(collect_iter(it).len(), 0);
    let mut root = PageId::NONE;
    for k in [A, B, C] {
        root = w.put(root, k, k).expect("put").root;
    }
    let cases: Vec<(Bound<&[u8]>, Bound<&[u8]>, usize)> = vec![
        (Bound::Included(B), Bound::Excluded(B), 0),
        (Bound::Excluded(B), Bound::Excluded(B), 0),
        (Bound::Excluded(B), Bound::Included(B), 0),
        (Bound::Included(B), Bound::Included(B), 1),
        (Bound::Included(C), Bound::Included(A), 0),
        (Bound::Excluded(C), Bound::Unbounded, 0),
        (Bound::Unbounded, Bound::Excluded(A), 0),
        (Bound::Unbounded, Bound::Unbounded, 3),
    ];
    for (lo, hi, n) in cases {
        let it = w.scan(root, lo, hi).expect("degenerate range scan");
        assert_eq!(collect_iter(it).len(), n, "range {lo:?}..{hi:?}");
    }
    publish_and_commit(w, root);
    assert_doctor_clean(&c);
}

// ---------------------------------------------------------------------------------------------
// 4. sequential_append_fill
// ---------------------------------------------------------------------------------------------

#[test]
fn sequential_append_fill() {
    const N: u64 = 100_000;
    let (_vfs, c, _opts) = fresh_with_page_size(8192);
    let mut w = c.begin_write().expect("begin_write");
    let mut root = PageId::NONE;
    let mut key = [0u8; 16];
    for i in 0..N {
        // 16-byte ascending key: big-endian counter followed by constant padding.
        key[..8].copy_from_slice(&i.to_be_bytes());
        key[8..].copy_from_slice(&[0xA5; 8]);
        root = w.put(root, &key, &i.to_le_bytes()).expect("sequential put").root;
    }
    publish_and_commit(w, root);

    let resolver = PrefixU64Roots::new(b"R");
    let rep = c.space_report(&resolver).expect("space_report");
    // The directory is a single small leaf; everything else is the data tree.
    assert!(rep.kinds.leaf >= 2, "expected data leaves plus the directory leaf: {:?}", rep.kinds);
    let data_leaves = rep.kinds.leaf - 1;
    // Leaf encoding (btree/node.rs): 4-byte header + per entry slot(2) + cell overhead(7) + key + value.
    let entry_bytes: u64 = 2 + 7 + 16 + 8;
    let encoded_total = 4 * data_leaves + N * entry_bytes;
    let capacity = u64::from(rep.page_size) - 32; // node_capacity(ps) = ps - 32
    let avg_fill = encoded_total as f64 / (data_leaves * capacity) as f64;
    assert!(
        avg_fill >= 0.90,
        "sequential append leaves only {:.1}% full on average ({} leaves, {} entries)",
        avg_fill * 100.0,
        data_leaves,
        N
    );
    // Content sanity: count, order and values.
    let snap = c.snapshot();
    let t = snap.tree(root).expect("tree");
    let mut n = 0u64;
    let mut prev: Option<Vec<u8>> = None;
    for item in t.scan(Bound::Unbounded, Bound::Unbounded).expect("scan") {
        let (k, v) = item.expect("scan item");
        assert_eq!(v, n.to_le_bytes().to_vec());
        if let Some(p) = &prev {
            assert!(p < &k, "scan out of order");
        }
        prev = Some(k);
        n += 1;
    }
    assert_eq!(n, N);
    drop(t);
    drop(snap);
    assert_doctor_clean(&c);
}

// ---------------------------------------------------------------------------------------------
// 5. delete_all_returns_null_root
// ---------------------------------------------------------------------------------------------

/// Deterministic Fisher-Yates driven by an LCG so the order is reproducible from the proptest seed.
fn shuffled<T>(items: &mut [T], mut seed: u64) {
    for i in (1..items.len()).rev() {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let j = ((seed >> 33) as usize) % (i + 1);
        items.swap(i, j);
    }
}

fn run_delete_all(entries: &[(Vec<u8>, Vec<u8>)], seed: u64, commit_between: bool) -> Result<(), TestCaseError> {
    let (vfs, c, opts) = fresh();
    let mut model = Model::new();
    let mut root = PageId::NONE;
    let mut w = c.begin_write().expect("begin_write");
    for (k, v) in entries {
        apply_op(&mut w, &mut root, &mut model, &Op::Put(k.clone(), v.clone()))?;
    }
    if commit_between {
        publish_and_commit(w, root);
        w = c.begin_write().expect("begin_write");
    }
    let mut keys: Vec<Vec<u8>> = model.keys().cloned().collect();
    shuffled(&mut keys, seed);
    // Deleting an absent key never changes the root.
    let absent = b"zzz-absent".to_vec();
    let r = w.delete(root, &absent).expect("delete absent key");
    prop_assert!(!r.existed);
    prop_assert_eq!(r.root, root);
    for k in &keys {
        apply_op(&mut w, &mut root, &mut model, &Op::Del(k.clone()))?;
        if !model.is_empty() {
            prop_assert!(root != PageId::NONE, "tree with {} keys left has the null root", model.len());
        }
    }
    prop_assert_eq!(root, PageId::NONE, "deleting every key must return the null root");
    let r = w.delete(root, &absent).expect("delete from empty tree");
    prop_assert!(!r.existed);
    prop_assert_eq!(r.root, PageId::NONE);
    prop_assert_eq!(collect_iter(w.scan(root, Bound::Unbounded, Bound::Unbounded).expect("scan")).len(), 0);
    publish_and_commit(w, root);
    assert_doctor_clean(&c);
    c.close().expect("close");

    let c2 = reopen(&vfs, &opts);
    let root2 = read_published_root(&c2);
    prop_assert_eq!(root2, PageId::NONE);
    {
        let snap = c2.snapshot();
        let t = snap.tree(root2).expect("null tree");
        check_tree(&t, &Model::new(), &[b"a".to_vec()])?;
    }
    assert_doctor_clean(&c2);
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn delete_all_returns_null_root(
        entries in prop::collection::vec((key_strategy(), value_strategy()), 1..400),
        seed in any::<u64>(),
        commit_between in any::<bool>(),
    ) {
        run_delete_all(&entries, seed, commit_between)?;
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    #[ignore = "heavy: 256 cases of up to 3000 entries"]
    fn delete_all_returns_null_root_heavy(
        entries in prop::collection::vec((key_strategy(), value_strategy()), 1..3000),
        seed in any::<u64>(),
        commit_between in any::<bool>(),
    ) {
        run_delete_all(&entries, seed, commit_between)?;
    }
}

// ---------------------------------------------------------------------------------------------
// 6. doctor_clean (dedicated multi-commit scenario; every other test also ends with a Full verify)
// ---------------------------------------------------------------------------------------------

#[test]
fn doctor_clean() {
    let (vfs, c, opts) = fresh();
    let probes: Vec<Vec<u8>> = vec![b"a".to_vec(), b"abc".to_vec(), Vec::new()];
    let mut model = Model::new();
    let mut root = PageId::NONE;
    let mut seed = 12345u64;
    let mut next = move || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 33) as usize
    };

    assert_doctor_clean(&c); // empty container
    for round in 0..8usize {
        let mut w = c.begin_write().expect("begin_write");
        for _ in 0..300 {
            let klen = next() % 6;
            let key: Vec<u8> = (0..klen).map(|_| 97 + (next() % 3) as u8).collect();
            let op = if next() % 4 == 0 {
                Op::Del(key)
            } else {
                let vlen = match next() % 8 {
                    0 => 1000 + next() % 5000, // overflow chains
                    1..=3 => next() % 600,
                    _ => next() % 40,
                };
                Op::Put(key, vec![(round as u8).wrapping_add(vlen as u8); vlen])
            };
            apply_op(&mut w, &mut root, &mut model, &op).expect("apply op");
        }
        // Overwrite a few values, flipping between overflow and inline representations.
        for k in model.keys().take(5).cloned().collect::<Vec<_>>() {
            let big = round % 2 == 0;
            let v = vec![7u8; if big { 4000 } else { 3 }];
            apply_op(&mut w, &mut root, &mut model, &Op::Put(k, v)).expect("apply overwrite");
        }
        publish_and_commit(w, root);
        assert_doctor_clean(&c);
        let rep = c.verify(&PrefixU64Roots::new(b"R"), VerifyLevel::Roots).expect("verify Roots");
        assert!(!rep.issues.iter().any(|i| matches!(i.severity, IssueSeverity::Error)));
        let snap = c.snapshot();
        let t = snap.tree(root).expect("tree");
        check_tree(&t, &model, &probes).expect("model matches after commit");
    }

    // Delete everything, verify, reopen, verify.
    let mut w = c.begin_write().expect("begin_write");
    for k in model.keys().cloned().collect::<Vec<_>>() {
        let r = w.delete(root, &k).expect("delete");
        assert!(r.existed);
        root = r.root;
    }
    assert_eq!(root, PageId::NONE);
    publish_and_commit(w, root);
    assert_doctor_clean(&c);
    c.close().expect("close");
    let c2 = reopen(&vfs, &opts);
    assert_doctor_clean(&c2);
}
