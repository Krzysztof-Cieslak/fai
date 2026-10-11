//! Native peers for materialized numeric, linked, graph and cardinality fixtures.

use std::collections::{HashMap, HashSet};

#[inline(never)]
fn sum_int(values: &[i64]) -> i64 {
    values.iter().sum::<i64>() + values.len() as i64
}
#[inline(never)]
fn sum_float(values: &[f64]) -> f64 {
    values.iter().sum::<f64>() + values.len() as f64
}

/// A retained numeric array crossing a whole-array consumer boundary.
pub fn array_int(n: i64) -> i64 {
    sum_int(&(0..n).collect::<Vec<_>>())
}

/// Inline Float elements, consumed after the entire array has been initialized.
pub fn array_float(n: i64) -> f64 {
    sum_float(&(0..n).map(|i| i as f64 + 0.25).collect::<Vec<_>>())
}

/// Retain the original array while updating an independent copy.
pub fn shared_arrays(n: i64) -> i64 {
    let original: Vec<_> = (0..n).collect();
    let mut changed = original.clone();
    for value in &mut changed {
        *value += 1;
    }
    sum_int(&original) + sum_int(&changed)
}

struct Node {
    value: i64,
    next: Option<Box<Node>>,
}
struct List(Option<Box<Node>>);
impl Drop for List {
    fn drop(&mut self) {
        while let Some(mut node) = self.0.take() {
            self.0 = node.next.take();
        }
    }
}
#[inline(never)]
fn sum_list(list: &List) -> i64 {
    let mut node = list.0.as_deref();
    let mut total = 0;
    while let Some(current) = node {
        total += current.value + 1;
        node = current.next.as_deref();
    }
    total
}

/// A linked list whose empty value allocates nothing and whose drop is iterative.
pub fn list(n: i64) -> i64 {
    let mut list = List(None);
    for value in 0..n {
        list.0 = Some(Box::new(Node { value, next: list.0.take() }));
    }
    sum_list(&list)
}

struct Tree {
    value: i64,
    left: Option<Box<Tree>>,
    right: Option<Box<Tree>>,
}
fn tree_make(depth: i64) -> Option<Box<Tree>> {
    (depth > 0).then(|| {
        Box::new(Tree { value: depth, left: tree_make(depth - 1), right: tree_make(depth - 1) })
    })
}
#[inline(never)]
fn tree_sum(tree: &Option<Box<Tree>>) -> i64 {
    tree.as_ref().map_or(0, |node| node.value + 1 + tree_sum(&node.left) + tree_sum(&node.right))
}

/// A complete tree of nonempty depth n, read after construction.
pub fn tree(depth: i64) -> i64 {
    tree_sum(&tree_make(depth))
}

#[inline(never)]
fn utf8_length(text: &str) -> i64 {
    text.chars().count() as i64
}

/// Grow a UTF-8 buffer by four scalars per iteration, then consume it.
pub fn utf8(n: i64) -> i64 {
    let mut text = String::new();
    for _ in 0..n {
        text.push_str("aéλ😀");
    }
    utf8_length(&text)
}

/// Four update passes over n distinct hash-map keys, then inspect both endpoints.
pub fn histogram(n: i64) -> i64 {
    if n <= 0 {
        return 0;
    }
    let mut map = HashMap::new();
    for i in 0..4 * n {
        *map.entry(i % n).or_insert(0i64) += 1;
    }
    map.len() as i64 + map[&0] + map[&(n - 1)]
}

/// Four insertion passes over n distinct keys, then inspect both endpoints.
pub fn set(n: i64) -> i64 {
    if n <= 0 {
        return 0;
    }
    let mut set = HashSet::new();
    for i in 0..4 * n {
        set.insert(i % n);
    }
    set.len() as i64 + i64::from(set.contains(&0)) + i64::from(set.contains(&(n - 1)))
}

#[inline(never)]
fn graph_walk(graph: &[Vec<i64>]) -> i64 {
    if graph.is_empty() {
        return 0;
    }
    let mut visited = vec![false; graph.len()];
    visited[0] = true;
    let mut queue = vec![0];
    let mut cursor = 0;
    while cursor < queue.len() {
        let node = queue[cursor];
        for &next in &graph[node] {
            if !visited[next as usize] {
                visited[next as usize] = true;
                queue.push(next as usize);
            }
        }
        cursor += 1;
    }
    cursor as i64
}

/// Build exactly three ordered edges per node, then perform array-queue BFS.
pub fn graph(n: i64) -> i64 {
    let graph: Vec<_> =
        (0..n).map(|i| vec![(i + 1) % n, (i * 2 + 1) % n, (i * 3 + 2) % n]).collect();
    graph_walk(&graph)
}

#[derive(Clone, Copy)]
struct Body {
    x: f64,
    y: f64,
    vx: f64,
    vy: f64,
}
#[inline(never)]
fn body_sum(bodies: &[Body]) -> f64 {
    bodies.iter().map(|body| body.x + body.y).sum()
}

/// Four pure maps over an input-sized array of four-field Float bodies.
pub fn bodies(n: i64) -> f64 {
    let mut bodies: Vec<_> =
        (0..n).map(|i| Body { x: i as f64, y: 0.0, vx: 0.25, vy: 0.5 }).collect();
    for _ in 0..4 {
        bodies = bodies
            .into_iter()
            .map(|body| Body { x: body.x + body.vx, y: body.y + body.vy, ..body })
            .collect();
    }
    body_sum(&bodies)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn materialized_integer_checksum() {
        assert_eq!(array_int(64), 64 * 65 / 2);
    }
    #[test]
    fn materialized_float_checksum() {
        assert_eq!(array_float(64), 64.0 * 63.0 / 2.0 + 80.0);
    }
    #[test]
    fn both_array_snapshots_contribute() {
        assert_eq!(shared_arrays(64), 64 * 66);
    }
    #[test]
    fn list_checksum_counts_cells() {
        assert_eq!(list(64), 64 * 65 / 2);
    }
    #[test]
    fn tree_checksum_counts_values_and_nodes() {
        assert_eq!(tree(12), 3 * (1 << 12) - 12 - 3);
    }
    #[test]
    fn utf8_counts_scalars_not_bytes() {
        assert_eq!(utf8(64), 256);
    }
    #[test]
    fn histogram_grows_distinct_keys() {
        assert_eq!(histogram(64), 72);
    }
    #[test]
    fn set_grows_distinct_keys() {
        assert_eq!(set(64), 66);
    }
    #[test]
    fn graph_visits_every_materialized_node() {
        assert_eq!(graph(64), 64);
    }
    #[test]
    fn empty_graph_never_divides_by_zero() {
        assert_eq!(graph(0), 0);
    }
    #[test]
    fn body_count_is_independent_of_map_count() {
        assert_eq!(bodies(64), 64.0 * 63.0 / 2.0 + 192.0);
    }
}
