//! Structurally matched binary-tree lookup workloads and explicit map controls.

use std::collections::BTreeMap;

/// Fixed key count of the original OptionTreeFind workload.
pub const KEYS: i64 = 1000;
/// Query counts for compute-sized and delivered-binary-sized measurements.
pub const QUERIES: &[i64] = &[5000, 100_000];

#[derive(Debug)]
#[repr(C)]
struct Node {
    left: Option<Box<Node>>,
    key: i64,
    value: i64,
    right: Option<Box<Node>>,
}

/// Four-field binary nodes, uniquely owned during insertion and borrowed for
/// lookup. Insertion order matches the Fai sample's midpoint/left/right walk.
#[derive(Debug, Default)]
pub struct BinaryTree {
    root: Option<Box<Node>>,
}

impl BinaryTree {
    /// Builds keys `[0, count)`, each mapped to `key * 3`, midpoint first.
    pub fn build(count: i64) -> Self {
        fn insert(node: &mut Option<Box<Node>>, key: i64, value: i64) {
            match node {
                None => *node = Some(Box::new(Node { left: None, key, value, right: None })),
                Some(node) if key < node.key => insert(&mut node.left, key, value),
                Some(node) if key > node.key => insert(&mut node.right, key, value),
                Some(node) => node.value = value,
            }
        }
        fn insert_range(root: &mut Option<Box<Node>>, lo: i64, hi: i64) {
            if lo >= hi {
                return;
            }
            let mid = lo + (hi - lo) / 2;
            insert(root, mid, mid * 3);
            insert_range(root, lo, mid);
            insert_range(root, mid + 1, hi);
        }
        let mut tree = Self::default();
        insert_range(&mut tree.root, 0, count.max(0));
        tree
    }

    /// Looks up one key without modifying or rebuilding the tree.
    pub fn find(&self, key: i64) -> Option<i64> {
        let mut current = self.root.as_deref();
        while let Some(node) = current {
            if key < node.key {
                current = node.left.as_deref();
            } else if key > node.key {
                current = node.right.as_deref();
            } else {
                return Some(node.value);
            }
        }
        None
    }

    /// Preorder shape encoding: `-1` for a leaf; key, value, left, right for a node.
    pub fn shape(&self) -> Vec<i64> {
        fn walk(node: Option<&Node>, output: &mut Vec<i64>) {
            match node {
                None => output.push(-1),
                Some(node) => {
                    output.extend([node.key, node.value]);
                    walk(node.left.as_deref(), output);
                    walk(node.right.as_deref(), output);
                }
            }
        }
        let mut result = Vec::new();
        walk(self.root.as_deref(), &mut result);
        result
    }

    /// Node count and height, used in untimed shape validation.
    pub fn dimensions(&self) -> (usize, usize) {
        fn walk(node: Option<&Node>) -> (usize, usize) {
            let Some(node) = node else { return (0, 0) };
            let (ln, lh) = walk(node.left.as_deref());
            let (rn, rh) = walk(node.right.as_deref());
            (ln + rn + 1, lh.max(rh) + 1)
        }
        walk(self.root.as_deref())
    }

    /// Position-weighted results for the fixed `i % 2000` query sequence.
    pub fn checksum(&self, queries: i64) -> i64 {
        checksum(queries, |key| self.find(key))
    }
}

/// Weights hits and misses (`-1`) by query position, making answer order visible
/// to the aggregate benchmark check.
pub fn checksum(queries: i64, mut find: impl FnMut(i64) -> Option<i64>) -> i64 {
    (0..queries).fold(0i64, |sum, i| {
        sum.wrapping_add((i + 1).wrapping_mul(find(i % (KEYS * 2)).unwrap_or(-1)))
    })
}

/// Independent answer oracle for the fixed key/value set.
pub fn expected(queries: i64) -> i64 {
    checksum(queries, |key| (key < KEYS).then_some(key * 3))
}

/// The original bulk-built BTreeMap application alternative, explicitly named.
pub fn btree() -> BTreeMap<i64, i64> {
    (0..KEYS).map(|k| (k, k * 3)).collect()
}

/// Original Fai tree implementation with public untimed validation and benchmark
/// entries. Only visibility and harness declarations are added to the sample.
pub fn fai_source() -> String {
    let original = include_str!("../../../samples/algorithms/OptionTreeFind.fai");
    let prefix = original.split("public main :").next().unwrap();
    format!(
        "{}\npublic make : Int -> Tree\nlet make n = buildBalanced 0 n Leaf\npublic answer : Tree -> Int -> Option Int\nlet answer tree key = find key tree\npublic shape : Tree -> List Int\nlet shape tree =\n  match tree with\n  | Leaf -> [-1]\n  | Node l k v r -> k :: v :: List.append (shape l) (shape r)\npublic count : Tree -> Int\nlet count tree = match tree with\n  | Leaf -> 0\n  | Node l k v r -> 1 + count l + count r\npublic height : Tree -> Int\nlet height tree = match tree with\n  | Leaf -> 0\n  | Node l k v r ->\n    let a = height l\n    let b = height r\n    1 + (if a > b then a else b)\ncheckedFrom : Tree -> Int -> Int -> Int -> Int\nlet checkedFrom tree i n acc =\n  if i >= n then acc else\n    match find (i % 2000) tree with\n    | None -> checkedFrom tree (i + 1) n (acc - (i + 1))\n    | Some value -> checkedFrom tree (i + 1) n (acc + (i + 1) * value)\npublic probe : Tree -> Int -> Int\nlet probe tree n = checkedFrom tree 0 n 0\npublic buildProbe : Int -> Int\nlet buildProbe n = probe (make 1000) n\npublic main : Runtime -> Unit\nlet main r = ()\n",
        prefix.replace("\ntype Tree =", "\npublic type Tree =")
    )
}

/// A line-protocol native worker. Lookup mode builds before the ready handshake;
/// build mode constructs one tree per request. Both preserve the fixed queries.
pub fn fai_worker(build: bool) -> String {
    let source = fai_source();
    let prefix = source.split("public main :").next().unwrap();
    let query = if build { "buildProbe n" } else { "probe tree n" };
    let tree = if build { "Leaf" } else { "make 1000" };
    format!(
        "{prefix}\nserve : Console -> Tree -> Unit / {{ Console }}\nlet serve console tree =\n  match console.readLine () with\n  | Ok (Some line) ->\n    match Int.fromString line with\n    | None -> ()\n    | Some n ->\n      let result = {query}\n      let _ = console.writeLine (Int.toString result)\n      serve console tree\n  | _ -> ()\npublic main : Runtime -> Unit / {{ Console }}\nlet main r =\n  let tree = {tree}\n  let _ = r.console.writeLine \"ready\"\n  serve r.console tree\n"
    )
}
