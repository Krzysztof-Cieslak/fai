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

/// Residual implementation differences for each independently interpreted pair.
/// Algorithm/staging agreement belongs to `Contract`; these costs remain native.
pub struct PeerNotes {
    /// Fai versus Rust.
    pub rust: &'static str,
    /// Fai versus OCaml.
    pub ocaml: &'static str,
}

/// Pair-specific qualifications retained in the machine-readable workload catalog.
pub fn peer_notes(name: &str) -> Option<PeerNotes> {
    for_module(name)?;
    let (rust, ocaml) = match name {
        "DictHistogram" | "GameOfLife" => (
            "Fai structural hash/linear probing/separate entries versus Rust seeded SwissTable/inline entries; both update through one entry lookup. Fai public folds currently materialize lists; Rust iterates directly. Empty-start minimum capacity and growth remain native.",
            "Fai linear probing versus OCaml chained buckets; OCaml uses lookup then replacement for updates. Fai public folds currently materialize lists; OCaml iterates directly. Empty-start minimum capacity, growth and GC remain native.",
        ),
        "MemoryHistogram" => (
            "Four empty-start entry-update passes and endpoint reads; Fai structural hash/linear probing/separate entries differ from Rust seeded SwissTable/inline entries and its growth policy.",
            "Four empty-start update passes and endpoint reads; Fai performs one entry update, while OCaml Hashtbl uses lookup then replacement. Hashing, chained buckets, minimum capacity/growth and GC differ.",
        ),
        "SetDedup" => (
            "Fai structural hash/linear probing/separate entries versus Rust seeded SwissTable/inline entries. All tables start empty without input-size reservation. Fai set folds currently materialize lists; traversal, native capacity/growth and optional-value layout remain visible.",
            "Fai linear probing versus OCaml chained buckets. All tables start empty without input-size reservation. Fai set folds currently materialize lists; traversal, native capacity/growth, optional-value layout and GC remain visible.",
        ),
        "MemorySet" | "UnionFind" | "OptionPath" | "GraphBFS" => (
            "Fai structural hash/linear probing/separate entries versus Rust seeded SwissTable/inline entries. All tables start empty without input-size reservation. Native minimum capacity/growth and optional-value layout remain visible.",
            "Fai linear probing versus OCaml chained buckets. All tables start empty without input-size reservation. Native minimum capacity/growth, optional-value layout and GC remain visible.",
        ),
        "FibMemo" => (
            "Shared recurrence/order and empty-start memo; Fai HashDict state passing and native probing/layout differ from Rust HashMap with borrowed mutation.",
            "Shared recurrence/order and empty-start memo; Fai HashDict differs from OCaml Hashtbl. OCaml uses boxed Int64 for required full-width wrapping; GC remains native.",
        ),
        "StringSlice" | "TailViewsAscii" | "TailViewsUnicode" => (
            "Fai reference-counted views/copies and ASCII evidence versus borrowed Rust str slices and scalar scans; the prefix array is retained through its consumer.",
            "Fai reference-counted views/copies and ASCII evidence versus copied OCaml substrings and scalar scans. Buffer.contents freezes by copying; GC remains native.",
        ),
        "WordCount" => (
            "Owned word arrays and split arrays on both sides; native decimal conversion, joining, split scanning/reservation and scalar-length implementations differ.",
            "Word arrays and split arrays on both sides; OCaml uses a sized join buffer and a count/fill array split. Fai/Rust use their array-producing string operations. Scalar-length scanning and GC remain native.",
        ),
        "StringBuild" | "MemoryUtf8" | "TailBuildAscii" | "TailBuildUnicode"
        | "TailLengthAscii" | "TailLengthUnicode" => (
            "Unreserved incremental string builders; native capacity growth, ASCII evidence and Unicode-scalar counting differ.",
            "Unreserved incremental builders; OCaml Buffer.contents freezes by copying. Native capacity growth, scalar counting and GC remain visible.",
        ),
        "ExprEval" | "JsonSerialize" | "Fannkuch" | "ListSort" => (
            "Shared linked logical structures and phase order; Rust monomorphic enums/Rc and Fai uniform cells differ in layout. Last-use ownership, compiler fusion/reuse and native buffer policies remain visible.",
            "Shared linked logical structures and phase order; GC versus last-use reference counting, native ADT layout and buffer policies remain visible.",
        ),
        _ => (
            "Shared logical algorithm; native scalar/data layout, ownership, allocation and legal compiler transformations remain visible.",
            "Shared logical algorithm; native scalar/data layout, GC/allocation and legal compiler transformations remain visible.",
        ),
    };
    Some(PeerNotes { rust, ocaml })
}

/// Untimed input windows cover empty values, representation thresholds and
/// algorithm boundaries independently of the measured sizes.
pub fn validation_inputs(name: &str) -> &'static [i64] {
    match name {
        "MergeSort" | "ListSort" | "QuickSort" => &[0, 1, 2, 3, 17, 31, 32, 33, 63, 64, 65, 257],
        "StringBuild" | "StringSlice" => &[0, 1, 2, 3, 31, 32, 63, 64, 65, 257],
        "WordCount" => &[0, 1, 9, 10, 11, 99, 100, 101],
        "InterfaceDispatch" => &[0, 1, 2, 3, 4, 5, 6, 31, 32],
        "ExprEval" => &[0, 1, 2, 3, 4, 5, 31, 32, 127, 128, 1024],
        "Fannkuch" => &[0, 1, 2, 3, 4, 5, 6],
        "FibMemo" => &[0, 1, 2, 3, 10, 93, 94],
        "GameOfLife" => &[0, 1, 2, 3, 5, 10],
        "OptionTreeFind" => &[0, 1, 999, 1000, 1001, 1999, 2000, 2001],
        "JsonSerialize" => &[0, 1, 2, 3, 4, 7, 8, 15, 16, 17, 63, 64, 65, 257],
        "SetDedup" => &[0, 1, 5, 6, 7, 23, 24, 25, 999, 1000, 1001],
        "DictHistogram" | "UnionFind" | "OptionPath" | "GraphBFS" => {
            &[0, 1, 5, 6, 7, 23, 24, 25, 95, 96, 97, 257]
        }
        _ => &[2, 3],
    }
}

/// Returns a reviewed contract; unregistered names never acquire eligibility.
#[must_use]
pub fn for_module(name: &str) -> Option<Contract> {
    if let Some(contract) = crate::memory_workloads::contract(name) {
        return Some(contract);
    }
    let (rust_entry, ocaml_entry, ocaml_support): (_, _, &[_]) = match name {
        "Fib" => ("algorithms::fib", "fib", &[]),
        "Collatz" => ("algorithms::collatz_sum", "collatz_sum", &[]),
        "MapSum" => ("algorithms::map_sum", "map_sum", &[]),
        "MergeSort" => (
            "algorithms::merge_sort_sum",
            "merge_sort_sum",
            &["sort_input", "merge_buffers", "merge_sort_values"],
        ),
        "BinaryTrees" => ("algorithms::tree_count", "tree_count", &["tree"]),
        "Pi" => ("algorithms::pi", "pi", &[]),
        "DictHistogram" => ("algorithms::dict_histogram", "dict_histogram", &[]),
        "WordCount" => (
            "algorithms::word_count",
            "word_count",
            &["unicode_length", "array_string_lengths", "join_words", "split_words", "word_text"],
        ),
        "MapSumShared" => ("algorithms::map_sum_shared", "map_sum_shared", &[]),
        "SetDedup" => ("algorithms::set_dedup", "set_dedup", &[]),
        "FoldPipeline" => ("algorithms::fold_pipeline", "fold_pipeline", &[]),
        "InterfaceDispatch" => {
            ("algorithms::interface_dispatch", "interface_dispatch", &["scorer"])
        }
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
        "ExprEval" => ("algorithms::expr_eval", "expr_eval", &["token", "expr", "eval_work"]),
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
        "JsonSerialize" => (
            "algorithms::json_serialize",
            "json_serialize",
            &["json", "unicode_length", "json_text"],
        ),
        "StringBuild" => {
            ("algorithms::string_build", "string_build", &["unicode_length", "string_build_text"])
        }
        "StringSlice" => (
            "algorithms::string_slice",
            "string_slice",
            &["unicode_length", "array_string_lengths", "slice_base", "unicode_prefix"],
        ),
        "OptionEval" => ("algorithms::option_eval", "option_eval", &[]),
        "IntEval" => ("algorithms::int_eval", "int_eval", &[]),
        "OptionPath" => ("algorithms::option_path", "option_path", &[]),
        "OptionTreeFind" => (
            "algorithms::option_tree_find",
            "option_tree_find",
            &["lookup_tree", "lookup_insert", "lookup_build", "lookup_find"],
        ),
        "ListSort" => (
            "algorithms::list_sort_sum",
            "list_sort_sum",
            &["sort_input", "merge_buffers", "linked_sort"],
        ),
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
        "MergeSort" => (
            Comparison::Kernel,
            "Shared Fisher-Yates input and stable bottom-up merge sort with zero-initialized scratch, cached run heads, identical pass order and position-weighted checksum.",
        ),
        "QuickSort" => (
            Comparison::Kernel,
            "Identical scrambled inputs, Lomuto partition, smaller-side-first recursion and position-weighted checksum on all peers.",
        ),
        "ListSort" => (
            Comparison::Kernel,
            "Shared shuffled input materialized as a linked list. Adjacent-run stable merges: linked through 32 elements, then two copied buffers and a linked result; the same bounded subtraction comparator.",
        ),
        "DictHistogram" | "SetDedup" | "GameOfLife" | "FibMemo" | "UnionFind" | "OptionPath" => (
            Comparison::Application,
            "Empty-start standard collections with the same key/update workload. Native hashing, probing, layout, minimum capacity and growth policies differ; folds use each collection's public traversal API.",
        ),
        "GraphBFS" => (
            Comparison::Application,
            "Empty-start standard hash collections with linked three-edge adjacency and linked frontiers. All stream adjacency lookup/visitation and prepend newly seen nodes; native hash-table implementations differ.",
        ),
        "JsonSerialize" => (
            Comparison::Kernel,
            "Build the same linked-child JSON tree with shared literal keys, then consume it into one unreserved output builder. Native ADT layout, reclamation and string freezing remain observable.",
        ),
        "WordCount" => (
            Comparison::Kernel,
            "Materialized input-word array, one joined string, then a materialized split array and whole-array scalar-length consumer. Completed input phases have no later uses; reclamation remains native.",
        ),
        "StringBuild" => (
            Comparison::Kernel,
            "Unreserved incremental appends of ab onto 0, then Unicode-scalar length. Native buffer growth, ASCII evidence and freezing/copying are implementation costs of the same string operations.",
        ),
        "StringSlice" => (
            Comparison::Kernel,
            "Incrementally construct the same base and retain an array of 200 half-length prefixes before a whole-array scalar-length consumer. Native views versus copies remain observable.",
        ),
        "OptionTreeFind" => (
            Comparison::Kernel,
            "Four-field binary nodes, midpoint/left/right insertion order, 1000 fixed keys, and identical half-hit query sequence. Native node layout and reclamation remain observable.",
        ),
        "ExprEval" => (
            Comparison::Kernel,
            "Back-to-front linked tokens consumed by recursive descent, then left-first iterative AST evaluation with linked work/value stacks. No retained token root; native enum layout and reclamation remain observable.",
        ),
        "Fannkuch" => (
            Comparison::Kernel,
            "Linked permutations and permutation collections. Construct producer groups left-to-right before flattening, then consume permutations through the same prefix-reversal idiom. Normal compiler fusion/reuse is allowed.",
        ),
        "InterfaceDispatch" => (
            Comparison::Kernel,
            "Select one of three immutable one-method dictionaries and invoke its score field. Known-choice devirtualization is allowed on every peer.",
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
        "MergeSort" => "stable-array-merges-v3",
        "QuickSort" => "matched-lomuto-v3",
        "ListSort" => "stable-list-merges-v3",
        "BinaryTrees" => "unallocated-leaves-v2",
        "NQueens" => "unallocated-empty-list-v2",
        "ExprEval" => "owned-linked-evaluator-v3",
        "Fannkuch" => "staged-permutations-v3",
        "WordCount" | "StringBuild" | "StringSlice" => "equivalent-string-phases-v3",
        "OptionTreeFind" => "binary-node-lookup-v3",
        "InterfaceDispatch" => "dictionary-choice-v3",
        "MapSumShared" => "materialized-shared-source-v2",
        "GraphBFS" | "DictHistogram" | "SetDedup" | "GameOfLife" | "FibMemo" | "UnionFind"
        | "OptionPath" => "empty-start-collections-v3",
        "JsonSerialize" => "linked-json-phases-v3",
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
            .chain(crate::memory_workloads::FIXTURES)
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
    fn sorting_comparisons_use_explicit_shared_algorithms() {
        assert_eq!(for_module("ListSort").unwrap().comparison, Comparison::Kernel);
    }

    #[test]
    fn utf8_builders_share_one_classification_policy() {
        assert_eq!(
            for_module("MemoryUtf8").unwrap().comparison,
            for_module("TailBuildUnicode").unwrap().comparison
        );
    }

    #[test]
    fn native_hash_policies_remain_application_comparisons() {
        assert_eq!(for_module("DictHistogram").unwrap().comparison, Comparison::Application);
        let notes = peer_notes("DictHistogram").unwrap();
        assert!(notes.rust.contains("one entry lookup"));
        assert!(notes.ocaml.contains("lookup then replacement"));
    }
}
