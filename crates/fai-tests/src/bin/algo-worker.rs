//! Persistent native Rust workload worker for warm AOT comparisons.

fn main() {
    let module = std::env::args().nth(1).expect("usage: algo-worker <module>");
    use fai_tests::algorithms::{self as algorithms, Oracle};
    use fai_tests::benchmark_aot::serve;
    let input = std::io::stdin();
    let output = std::io::stdout();
    macro_rules! dispatch {
        ($($name:literal => $kind:ident($function:ident)),* $(,)?) => {
            match module.as_str() {
                $($name => serve(Oracle::$kind(algorithms::$function), input.lock(), output.lock()),)*
                _ => panic!("unknown benchmark module: {module}"),
            }
        };
    }
    let result = dispatch! {
        "Fib" => Int(fib),
        "Collatz" => Int(collatz_sum),
        "MapSum" => Int(map_sum),
        "MergeSort" => Int(merge_sort_sum),
        "BinaryTrees" => Int(tree_count),
        "Pi" => Float(pi),
        "DictHistogram" => Int(dict_histogram),
        "WordCount" => Int(word_count),
        "MapSumShared" => Int(map_sum_shared),
        "SetDedup" => Int(set_dedup),
        "FoldPipeline" => Int(fold_pipeline),
        "InterfaceDispatch" => Int(interface_dispatch),
        "Particles" => Float(particles),
        "VecMat" => Float(vec_mat),
        "NQueens" => Int(nqueens),
        "MatrixMultiply" => Int(matrix_multiply),
        "FloatMatrixMultiply" => Float(float_matrix_multiply),
        "Levenshtein" => Int(levenshtein),
        "GameOfLife" => Int(game_of_life),
        "SpectralNorm" => Float(spectral_norm),
        "Mandelbrot" => Float(mandelbrot),
        "Ackermann" => Int(ackermann),
        "PrngXorshift" => Int(prng_xorshift),
        "ExprEval" => Int(expr_eval),
        "GraphBFS" => Int(graph_bfs),
        "CoinChange" => Int(coin_change),
        "FibMemo" => Int(fib_memo),
        "QuickSort" => Int(quicksort_sum),
        "Sieve" => Int(sieve),
        "NBody" => Float(nbody),
        "Fannkuch" => Int(fannkuch),
        "UnionFind" => Int(union_find),
        "JsonSerialize" => Int(json_serialize),
        "StringBuild" => Int(string_build),
        "StringSlice" => Int(string_slice),
        "OptionEval" => Int(option_eval),
        "IntEval" => Int(int_eval),
        "OptionPath" => Int(option_path),
        "OptionTreeFind" => Int(option_tree_find),
        "ListSort" => Int(list_sort_sum),
    };
    if let Err(error) = result {
        eprintln!("benchmark worker: {error}");
        std::process::exit(1);
    }
}
