//! Reviewed workload identities and comparison eligibility, independent of results.

/// Packaging and measurement protocol of isolated native peers.
pub const METHODOLOGY: &str = "isolated-aot-v2";

/// What a cross-language workload can establish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Comparison {
    /// The same algorithm and logical data structures, with normal optimization.
    Kernel,
    /// Library, representation, or staging choices also change the work.
    Application,
}

impl Comparison {
    /// Stable wire spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Kernel => "matched-kernel",
            Self::Application => "application",
        }
    }
}

/// Source-level contract shared by every peer of a registered workload.
#[derive(Debug, Clone, Copy)]
pub struct Contract {
    /// Version of the workload, separate from the compiler and packaging version.
    pub version: &'static str,
    /// Eligibility for matched-kernel summaries.
    pub comparison: Comparison,
    /// Algorithm, materialization, and remaining implementation differences.
    pub note: &'static str,
    /// Statically selected Rust function inside the workload crate.
    pub rust_entry: &'static str,
    /// Statically selected OCaml function or closed function expression.
    pub ocaml_entry: &'static str,
    /// Additional top-level OCaml declarations required by that function.
    pub ocaml_support: &'static [&'static str],
}

/// Returns a reviewed contract; unregistered names never acquire eligibility.
#[must_use]
pub fn for_module(name: &str) -> Option<Contract> {
    let (rust_entry, ocaml_entry, ocaml_support): (_, _, &[_]) = match name {
        "Fib" => ("algorithms::fib", "fib", &[]),
        "Collatz" => ("algorithms::collatz_sum", "collatz_sum", &[]),
        "MapSum" => ("algorithms::map_sum", "map_sum", &[]),
        "MergeSort" => ("algorithms::merge_sort_sum", "merge_sort_sum", &[]),
        "BinaryTrees" => ("algorithms::tree_count", "tree_count", &["tree"]),
        "Pi" => ("algorithms::pi", "pi", &[]),
        "DictHistogram" => ("algorithms::dict_histogram", "dict_histogram", &[]),
        "WordCount" => ("algorithms::word_count", "word_count", &[]),
        "MapSumShared" => ("algorithms::map_sum_shared", "map_sum_shared", &[]),
        "SetDedup" => ("algorithms::set_dedup", "set_dedup", &[]),
        "FoldPipeline" => ("algorithms::fold_pipeline", "fold_pipeline", &[]),
        "InterfaceDispatch" => ("algorithms::interface_dispatch", "interface_dispatch", &[]),
        "Particles" => ("algorithms::particles", "particles", &[]),
        "VecMat" => ("algorithms::vec_mat", "vec_mat", &[]),
        "NQueens" => ("algorithms::nqueens", "nqueens", &[]),
        "MatrixMultiply" => ("algorithms::matrix_multiply", "matrix_multiply", &[]),
        "FloatMatrixMultiply" => {
            ("algorithms::float_matrix_multiply", "float_matrix_multiply", &[])
        }
        "Levenshtein" => ("algorithms::levenshtein", "levenshtein", &[]),
        "GameOfLife" => ("algorithms::game_of_life", "game_of_life", &[]),
        "SpectralNorm" => ("algorithms::spectral_norm", "spectral_norm", &[]),
        "Mandelbrot" => ("algorithms::mandelbrot", "mandelbrot", &[]),
        "Ackermann" => ("algorithms::ackermann", "ackermann", &[]),
        "PrngXorshift" => ("algorithms::prng_xorshift", "prng_xorshift", &[]),
        "ExprEval" => ("algorithms::expr_eval", "expr_eval", &["token", "expr"]),
        "GraphBFS" => {
            ("algorithms::graph_bfs", "graph_bfs", &["graph_adjacency", "graph_reachable"])
        }
        "CoinChange" => ("algorithms::coin_change", "coin_change", &[]),
        "FibMemo" => ("algorithms::fib_memo", "fib_memo", &[]),
        "QuickSort" => ("algorithms::quicksort_sum", "quicksort_sum", &[]),
        "Sieve" => ("algorithms::sieve", "sieve", &[]),
        "NBody" => ("algorithms::nbody", "nbody", &[]),
        "Fannkuch" => ("algorithms::fannkuch", "fannkuch", &[]),
        "UnionFind" => ("algorithms::union_find", "union_find", &[]),
        "JsonSerialize" => ("algorithms::json_serialize", "json_serialize", &["json"]),
        "StringBuild" => ("algorithms::string_build", "string_build", &[]),
        "StringSlice" => ("algorithms::string_slice", "string_slice", &[]),
        "OptionEval" => ("algorithms::option_eval", "option_eval", &[]),
        "IntEval" => ("algorithms::int_eval", "int_eval", &[]),
        "OptionPath" => ("algorithms::option_path", "option_path", &[]),
        "OptionTreeFind" => ("algorithms::option_tree_find", "option_tree_find", &["IntMap"]),
        "ListSort" => ("algorithms::list_sort_sum", "list_sort_sum", &[]),
        "TailQuickSort" => ("tail_components::quicksort", "quicksort", &[]),
        "TailBuildAscii" => {
            ("tail_components::build_ascii", "(fun n -> length (ascii n))", &["ascii", "length"])
        }
        "TailBuildUnicode" => (
            "tail_components::build_unicode",
            "(fun n -> length (unicode n))",
            &["unicode", "length"],
        ),
        "TailLengthAscii" => (
            "tail_components::length_ascii",
            "(fun n -> lengths (ascii n))",
            &["ascii", "length", "lengths"],
        ),
        "TailLengthUnicode" => (
            "tail_components::length_unicode",
            "(fun n -> lengths (unicode n))",
            &["unicode", "length", "lengths"],
        ),
        "TailViewsAscii" => (
            "tail_components::views_ascii",
            "(fun n -> views (ascii n))",
            &["ascii", "length", "prefix", "views"],
        ),
        "TailViewsUnicode" => (
            "tail_components::views_unicode",
            "(fun n -> views (unicode n))",
            &["unicode", "length", "prefix", "views"],
        ),
        _ => return None,
    };
    let (comparison, note) = match name {
        "MergeSort" | "QuickSort" | "ListSort" => (
            Comparison::Application,
            "Different library sorts or input preparation; ListSort also uses Vec versus List. Use matched sorting kernels for compiler attribution.",
        ),
        "DictHistogram" | "SetDedup" | "GameOfLife" | "FibMemo" | "UnionFind" | "OptionPath" => (
            Comparison::Application,
            "Hash functions, table layout, initial capacity and growth policies differ.",
        ),
        "GraphBFS" => (
            Comparison::Application,
            "All peers build adjacency; hash policies and List versus Vec adjacency/frontiers differ.",
        ),
        "JsonSerialize" => (
            Comparison::Application,
            "All peers build the tree and serialize into one buffer; List versus Vec children and buffer policies differ.",
        ),
        "WordCount" => (
            Comparison::Application,
            "Word materialization, joining, splitting and retained source lifetimes differ.",
        ),
        "StringBuild" => (
            Comparison::Application,
            "Incremental append, but initial reservation and final buffer copying differ.",
        ),
        "StringSlice" => (
            Comparison::Application,
            "Base construction and materialized substring versus scalar scan differ; use TailViews for matched retained prefixes.",
        ),
        "OptionTreeFind" => (
            Comparison::Application,
            "Fai binary tree, Rust BTreeMap and OCaml Map; use tree_lookup for matched nodes.",
        ),
        "ExprEval" => (
            Comparison::Application,
            "Equivalent parser results; AST layout and explicit-stack versus recursive evaluation differ.",
        ),
        "Fannkuch" => (
            Comparison::Application,
            "Full permutation materialization, but concat-map versus fold-right staging differs.",
        ),
        "InterfaceDispatch" => (
            Comparison::Application,
            "Interface construction/dispatch versus a handwritten branch selector.",
        ),
        "BinaryTrees" => (
            Comparison::Kernel,
            "Build and count the same full binary tree; empty children allocate no cells on every peer. Runtime headers and allocator policy remain observable.",
        ),
        "MapSumShared" => (
            Comparison::Kernel,
            "One runtime-sized materialized integer array, traversed twice; no retained mapped intermediate.",
        ),
        "MapSum" | "FoldPipeline" => (
            Comparison::Kernel,
            "One arithmetic result over a runtime range; normal fusion and closed-form evaluation allowed. Allocation-free results have no memory-win ratio.",
        ),
        "Particles" | "NBody" => (
            Comparison::Kernel,
            "Five bodies, runtime iteration count and matching arithmetic order; physical record layout and reclamation remain observable.",
        ),
        "NQueens" => (
            Comparison::Kernel,
            "Backtracking with persistent linked placements and allocation-free empty lists.",
        ),
        "Sieve" => (
            Comparison::Kernel,
            "The same sieve over Boolean elements; byte versus word slots remain a representation difference.",
        ),
        "TailQuickSort" => (
            Comparison::Kernel,
            "Identical scrambled inputs, Lomuto partition, smaller-side-first recursion and position-weighted checksum.",
        ),
        name if name.starts_with("Tail") => (
            Comparison::Kernel,
            "Incrementally construct identical ASCII or UTF-8 input; count Unicode scalars and retain prefixes where specified. Runtime copying/view choices remain observable.",
        ),
        _ => (
            Comparison::Kernel,
            "Matching runtime inputs, logical data and arithmetic/evaluation order; normal compiler optimization is unrestricted.",
        ),
    };
    let version = match name {
        "BinaryTrees" => "unallocated-leaves-v2",
        "NQueens" | "ExprEval" | "Fannkuch" => "unallocated-empty-list-v2",
        "MapSumShared" => "materialized-shared-source-v2",
        "GraphBFS" => "materialized-adjacency-v2",
        "JsonSerialize" => "single-buffer-v2",
        "MapSum" | "FoldPipeline" => "unrestricted-arithmetic-v2",
        name if name.starts_with("Tail") => "tail-components-v1",
        _ => "reviewed-source-v1",
    };
    Some(Contract { version, comparison, note, rust_entry, ocaml_entry, ocaml_support })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registered_workload_has_an_explicit_contract() {
        let missing: Vec<_> = crate::algorithms::ALGORITHMS
            .iter()
            .chain(crate::tail_components::COMPONENTS)
            .filter(|case| for_module(case.module).is_none())
            .map(|case| case.module)
            .collect();
        assert!(missing.is_empty(), "missing contracts: {missing:?}");
    }

    #[test]
    fn unknown_workloads_cannot_enter_matched_summaries() {
        assert!(for_module("Unreviewed").is_none());
    }

    #[test]
    fn library_sort_comparisons_are_not_compiler_kernels() {
        assert_eq!(for_module("ListSort").unwrap().comparison, Comparison::Application);
    }
}
