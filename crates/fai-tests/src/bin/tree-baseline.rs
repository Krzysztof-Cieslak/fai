//! Persistent native binary-tree and BTreeMap benchmark controls.

use std::io::{self, BufRead, Write};

use fai_tests::tree_lookup::{self, BinaryTree, KEYS};

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let binary = args.first().is_none_or(|s| s == "binary");
    let rebuild = args.get(1).is_some_and(|s| s == "build");
    let tree = (!rebuild && binary).then(|| BinaryTree::build(KEYS));
    let map = (!rebuild && !binary).then(tree_lookup::btree);
    let mut out = io::BufWriter::new(io::stdout().lock());
    writeln!(out, "ready").unwrap();
    out.flush().unwrap();
    for line in io::stdin().lock().lines() {
        let n: i64 = line.unwrap().parse().expect("query count");
        let answer = if binary {
            match &tree {
                Some(tree) => tree.checksum(n),
                None => BinaryTree::build(KEYS).checksum(n),
            }
        } else {
            match &map {
                Some(map) => tree_lookup::checksum(n, |key| map.get(&key).copied()),
                None => {
                    let map = tree_lookup::btree();
                    tree_lookup::checksum(n, |key| map.get(&key).copied())
                }
            }
        };
        writeln!(out, "{answer}").unwrap();
        out.flush().unwrap();
    }
}
