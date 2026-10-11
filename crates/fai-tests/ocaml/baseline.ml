(* The OCaml side of the subprocess runtime/memory comparison: run one algorithm
   at one size and print the result.

   This is the OCaml twin of `src/bin/algo-baseline.rs`. The benchmark spawns it
   against the `fai build` binary (and the Rust release binary) so it compares a
   delivered, ocamlopt-compiled native executable to a delivered Fai binary
   (process startup, the workload, and exit). Printing the result keeps the
   computation from being optimized away, matching the other binaries' `main`.

   The implementations compute the same results but do not always use identical
   representations. Many pair an `array` with Fai's `Array`, or a persistent
    `list` with Fai's `List`. Hash workloads retain native `Hashtbl` policies;
    sorting, binary lookup, parser and permutation workloads use explicit shared
    algorithms. See docs/BENCHMARK.md for the comparison's scope.

   OCaml's native `int` is 63-bit, so the two workloads that depend on full 64-bit
   wrapping — `PrngXorshift` (u64 bit-twiddling) and `FibMemo` (i64 wrapping over
   thousands of Fibonacci sums) — use the `Int64` module to reproduce the oracle's
   two's-complement result; the rest fit native `int`.

   Usage: `baseline <module> <n>`. With `FAI_REPORT_RSS` set, it also prints its
   peak resident set size to stderr (`fai-peak-rss-kib: <n>`, the same line the Fai
   runtime and the Rust baseline emit), so the memory comparison harness can
   measure all three binaries the same way. *)

(* The first run of decimal digits in [s] as an int, or [None]. *)
let first_int s =
  let len = String.length s in
  let is_digit c = c >= '0' && c <= '9' in
  let i = ref 0 in
  while !i < len && not (is_digit s.[!i]) do
    incr i
  done;
  let j = ref !i in
  while !j < len && is_digit s.[!j] do
    incr j
  done;
  if !j > !i then Some (int_of_string (String.sub s !i (!j - !i))) else None

(* This process's peak resident set size in KiB, read from `/proc/self/status`
   (`VmHWM`, the high-water mark). Linux-only; yields [None] elsewhere (the file is
   absent, so the read raises and is caught). *)
let peak_rss_kib () =
  try
    let ic = open_in "/proc/self/status" in
    let result = ref None in
    (try
       while !result = None do
         let line = input_line ic in
         if String.length line >= 6 && String.sub line 0 6 = "VmHWM:" then result := first_int line
       done
     with End_of_file -> ());
    close_in ic;
    !result
  with _ -> None

(* The result of an algorithm: a native int, a 64-bit int (the wrapping
   workloads), or a float. *)
type result =
  | I of int
  | I64 of Int64.t
  | F of float

(* Naive recursive Fibonacci. *)
let rec fib n = if n < 2 then n else fib (n - 1) + fib (n - 2)

(* Sum of Collatz stopping times over `1..=n`. *)
let collatz_sum n =
  let steps_of start =
    let m = ref start and steps = ref 0 in
    while !m > 1 do
      m := (if !m mod 2 = 0 then !m / 2 else (3 * !m) + 1);
      incr steps
    done;
    !steps
  in
  let acc = ref 0 in
  for i = 1 to n do
    acc := !acc + steps_of i
  done;
  !acc

(* Sum of doubling every element of `[0, n)`. Every compiler may simplify the
   arithmetic series; there are no per-element optimization barriers. *)
let map_sum n =
  let acc = ref 0 in
  for x = 0 to n - 1 do
    acc := !acc + (x * 2)
  done;
  !acc

let sort_input pattern n =
  let n = max 0 n in
  let values = Array.init n (fun i -> match pattern with
    | 1 -> n - 1 - i
    | 3 -> 7
    | 4 -> i mod 4
    | 5 -> n - min n ((i / 32 + 1) * 32) + i mod 32
    | _ -> i) in
  if pattern = 2 then begin
    let state = ref 1 in
    for i = n - 1 downto 1 do
      state := (!state * 1664525 + 1013904223) land 2147483647;
      let j = !state mod (i + 1) in
      let value = values.(i) in values.(i) <- values.(j); values.(j) <- value
    done
  end;
  values

let merge_buffers (cmp : int -> int -> int) (initial : int array) (scratch : int array) =
  let n = Array.length initial in
  let source = ref initial and destination = ref scratch in
  let width = ref 1 in
  while !width < n do
    let low = ref 0 in
    while !low < n do
      let middle = min n (!low + !width) in
      let high = min n (middle + !width) in
      let left = ref !low and right = ref middle and out = ref !low in
      let a = ref (if !left < middle then (!source).(!left) else 0) in
      let b = ref (if !right < high then (!source).(!right) else 0) in
      while !left < middle && !right < high do
        if cmp !a !b <= 0 then begin
          (!destination).(!out) <- !a; incr left;
          if !left < middle then a := (!source).(!left)
        end else begin
          (!destination).(!out) <- !b; incr right;
          if !right < high then b := (!source).(!right)
        end;
        incr out
      done;
      while !left < middle do (!destination).(!out) <- (!source).(!left); incr left; incr out done;
      while !right < high do (!destination).(!out) <- (!source).(!right); incr right; incr out done;
      low := high
    done;
    let old = !source in source := !destination; destination := old;
    width := if !width >= n - !width then n else !width * 2
  done;
  !source

let merge_sort_values values =
  let n = Array.length values in
  if n < 2 then values else merge_buffers compare values (Array.make n 0)

(* Position-weighted checksum after the shared shuffle and bottom-up merge sort. *)
let merge_sort_sum n =
  let v = merge_sort_values (sort_input 2 n) in
  let answer = ref 0 in
  Array.iteri (fun i x -> answer := !answer + i * x) v;
  !answer

type tree =
  | Leaf
  | Node of tree * tree

(* The number of internal nodes in a full binary tree of the given depth. *)
let tree_count depth =
  let rec build d = if d <= 0 then Leaf else Node (build (d - 1), build (d - 1)) in
  let rec count = function Leaf -> 0 | Node (l, r) -> 1 + count l + count r in
  count (build depth)

(* The Leibniz approximation of pi from `terms` terms. *)
let pi terms =
  let acc = ref 0.0 in
  let i = ref 0 in
  while !i < terms do
    let denom = (2.0 *. float_of_int !i) +. 1.0 in
    acc := !acc +. (if !i mod 2 = 0 then 1.0 /. denom else -1.0 /. denom);
    incr i
  done;
  4.0 *. !acc

(* The count-weighted sum of a histogram of `n` keys (`i % 256`) in a hash map. *)
let dict_histogram n =
  let buckets = 256 in
  let counts = Hashtbl.create 0 in
  for i = 0 to n - 1 do
    let k = i mod buckets in
    let c = try Hashtbl.find counts k with Not_found -> 0 in
    Hashtbl.replace counts k (c + 1)
  done;
  Hashtbl.fold (fun key count acc -> acc + (key * count)) counts 0

let unicode_length text =
  let count = ref 0 in
  String.iter (fun ch -> if Char.code ch land 0xc0 <> 0x80 then incr count) text;
  !count

let[@inline never] array_string_lengths parts =
  Array.fold_left (fun total word -> total + unicode_length word) 0 parts

let join_words words =
  let count = Array.length words in
  let length = Array.fold_left (fun total word -> total + String.length word) (max 0 (count - 1)) words in
  let bytes = Bytes.create length in
  let offset = ref 0 in
  Array.iteri (fun i word ->
    if i > 0 then begin Bytes.set bytes !offset ' '; incr offset end;
    Bytes.blit_string word 0 bytes !offset (String.length word);
    offset := !offset + String.length word) words;
  Bytes.unsafe_to_string bytes

let split_words text =
  let count = ref 1 in
  String.iter (fun ch -> if ch = ' ' then incr count) text;
  let words = Array.make !count "" in
  let start = ref 0 and slot = ref 0 in
  String.iteri (fun i ch -> if ch = ' ' then begin
    words.(!slot) <- String.sub text !start (i - !start);
    incr slot; start := i + 1
  end) text;
  words.(!slot) <- String.sub text !start (String.length text - !start);
  words

let word_text n = join_words (Array.init (max 0 n) string_of_int)

(* Materialized input words, joined text, and materialized split output. *)
let word_count n = array_string_lengths (split_words (word_text n))

(* Two traversals of one materialized array, matching Fai's shared source.
   The mapped intermediate is fused into its fold on all three sides. *)
let map_sum_shared n =
  let xs = Array.init (max n 0) Fun.id in
  let doubled = Array.fold_left (fun acc x -> acc + (x * 2)) 0 xs in
  let original = Array.fold_left (fun acc x -> acc + x) 0 xs in
  doubled + original

(* The sum of the distinct values among `[0, n)` reduced modulo a bucket count. *)
let set_dedup n =
  let buckets = 1000 in
  let s = Hashtbl.create 0 in
  for i = 0 to n - 1 do
    Hashtbl.replace s (i mod buckets) ()
  done;
  Hashtbl.fold (fun k () acc -> acc + k) s 0

(* The sum over `[0, n)` of `((x + 1) * 2) + 3`. *)
let fold_pipeline n =
  let acc = ref 0 in
  for x = 0 to n - 1 do
    acc := !acc + (((x + 1) * 2) + 3)
  done;
  !acc

type scorer = { score : int -> int }

(* Select one of three one-method dictionaries; normal devirtualization is allowed. *)
let interface_dispatch n =
  let doubler = { score = (fun x -> x * 2) } in
  let incer = { score = (fun x -> x + 1) } in
  let negator = { score = (fun x -> -x) } in
  let pick i = match i mod 3 with 0 -> doubler | 1 -> incer | _ -> negator in
  let acc = ref 0 in
  for i = 0 to n - 1 do
    acc := !acc + (pick i).score i
  done;
  !acc

(* The position checksum of five particles after `n` semi-implicit Euler steps
   under a central spring force. The summation order matches the Fai fold. *)
let particles n =
  let dt = 0.01 in
  let bodies =
    Array.init 5 (fun k -> [| float_of_int k +. 1.0; float_of_int k +. 2.0; 0.0; 0.0 |])
  in
  for _ = 1 to n do
    Array.iter
      (fun b ->
        let nvx = b.(2) +. (dt *. (0.0 -. b.(0))) in
        let nvy = b.(3) +. (dt *. (0.0 -. b.(1))) in
        b.(0) <- b.(0) +. (dt *. nvx);
        b.(1) <- b.(1) +. (dt *. nvy);
        b.(2) <- nvx;
        b.(3) <- nvy)
      bodies
  done;
  let acc = ref 0.0 in
  Array.iter (fun b -> acc := !acc +. b.(0) +. b.(1)) bodies;
  !acc

(* A register-resident vector/matrix kinematics loop: each step squares a 2x2
   matrix, applies it to the running 2-vector, and accumulates the components. *)
let vec_mat n =
  let ra = 0.5 and rb = -0.25 and rc = 0.25 and rd = 0.5 in
  let vx = ref 1.0 and vy = ref 1.0 and acc = ref 0.0 in
  let i = ref 0 in
  while !i < n do
    let ma = (ra *. ra) +. (rb *. rc) in
    let mb = (ra *. rb) +. (rb *. rd) in
    let mc = (rc *. ra) +. (rd *. rc) in
    let md = (rc *. rb) +. (rd *. rd) in
    let wx = (ma *. !vx) +. (mb *. !vy) in
    let wy = (mc *. !vx) +. (md *. !vy) in
    acc := !acc +. wx +. wy;
    vx := wx;
    vy := wy;
    incr i
  done;
  !acc

(* The number of solutions to the `n`-queens puzzle. The placement so far is a
   persistent `list` of chosen columns (most recent first), matching the Fai
   sample's linked structure. *)
let nqueens n =
  let rec safe_from c d placed =
    match placed with
    | [] -> true
    | q :: rest -> q <> c && q - c <> d && c - q <> d && safe_from c (d + 1) rest
  in
  let rec solve size row placed =
    if row >= size then 1
    else begin
      let count = ref 0 in
      for c = 0 to size - 1 do
        if safe_from c 1 placed then count := !count + solve size (row + 1) (c :: placed)
      done;
      !count
    end
  in
  solve n 0 []

(* The sum of all entries of the `n`-by-`n` product `A·B`, `A(i,j)=(i+j)%7`,
   `B(i,j)=(i*j+1)%5`, over an array of rows. *)
let matrix_multiply n =
  let rows = Array.init n (fun i -> Array.init n (fun j -> (i + j) mod 7)) in
  let cols = Array.init n (fun j -> Array.init n (fun i -> ((i * j) + 1) mod 5)) in
  let acc = ref 0 in
  Array.iter
    (fun arow ->
      Array.iter
        (fun bcol ->
          let s = ref 0 in
          for k = 0 to n - 1 do
            s := !s + (arow.(k) * bcol.(k))
          done;
          acc := !acc + !s)
        cols)
    rows;
  !acc

(* The float twin of `matrix_multiply`, over unboxed `Array Float` rows/columns;
   folds left to match the Fai version's accumulation order. *)
let float_matrix_multiply n =
  let rows = Array.init n (fun i -> Array.init n (fun j -> float_of_int ((i + j) mod 7))) in
  let cols = Array.init n (fun j -> Array.init n (fun i -> float_of_int (((i * j) + 1) mod 5))) in
  let acc = ref 0.0 in
  Array.iter
    (fun arow ->
      let inner = ref 0.0 in
      Array.iter
        (fun bcol ->
          let d = ref 0.0 in
          for k = 0 to n - 1 do
            d := !d +. (arow.(k) *. bcol.(k))
          done;
          inner := !inner +. !d)
        cols;
      acc := !acc +. !inner)
    rows;
  !acc

(* The edit distance between two length-`n` integer sequences by a single-row DP. *)
let levenshtein n =
  let a = Array.init n (fun i -> i mod 7) in
  let b = Array.init n (fun i -> i * 3 mod 7) in
  let row = Array.init (n + 1) (fun i -> i) in
  for i = 0 to n - 1 do
    let diag = ref row.(0) in
    row.(0) <- i + 1;
    for j = 0 to n - 1 do
      let cost = if a.(i) <> b.(j) then 1 else 0 in
      let up = row.(j + 1) in
      row.(j + 1) <- min (row.(j) + 1) (min (up + 1) (!diag + cost));
      diag := up
    done
  done;
  row.(n)

(* The live-cell count after `n` Conway generations from the R-pentomino seed. *)
let game_of_life n =
  let offsets = [| (-1, -1); (0, -1); (1, -1); (-1, 0); (1, 0); (-1, 1); (0, 1); (1, 1) |] in
  let live = ref (Hashtbl.create 0) in
  List.iter (fun cell -> Hashtbl.replace !live cell ()) [ (1, 0); (2, 0); (0, 1); (1, 1); (1, 2) ];
  for _ = 1 to n do
    let counts = Hashtbl.create 0 in
    Hashtbl.iter
      (fun (x, y) () ->
        Array.iter
          (fun (dx, dy) ->
            let key = (x + dx, y + dy) in
            let c = try Hashtbl.find counts key with Not_found -> 0 in
            Hashtbl.replace counts key (c + 1))
          offsets)
      !live;
    let next = Hashtbl.create 0 in
    Hashtbl.iter
      (fun cell c -> if c = 3 || (c = 2 && Hashtbl.mem !live cell) then Hashtbl.replace next cell ())
      counts;
    live := next
  done;
  Hashtbl.length !live

(* The spectral-norm estimate of the Hilbert-like matrix after ten power
   iterations. All sums fold left to match the Fai version's order. *)
let spectral_norm n =
  let eval_a i j =
    let s = float_of_int (i + j) in
    1.0 /. ((s *. (s +. 1.0) /. 2.0) +. float_of_int i +. 1.0)
  in
  let mul_av u =
    Array.init n (fun i ->
        let acc = ref 0.0 in
        for j = 0 to n - 1 do
          acc := !acc +. (eval_a i j *. u.(j))
        done;
        !acc)
  in
  let mul_atv u =
    Array.init n (fun i ->
        let acc = ref 0.0 in
        for j = 0 to n - 1 do
          acc := !acc +. (eval_a j i *. u.(j))
        done;
        !acc)
  in
  let mul_at_av u = mul_atv (mul_av u) in
  let u = ref (Array.make n 1.0) in
  let v = ref (Array.copy !u) in
  for _ = 1 to 10 do
    v := mul_at_av !u;
    u := mul_at_av !v
  done;
  let dot xs ys =
    let acc = ref 0.0 in
    for i = 0 to n - 1 do
      acc := !acc +. (xs.(i) *. ys.(i))
    done;
    !acc
  in
  sqrt (dot !u !v /. dot !v !v)

(* The clamped-magnitude sum over an `n`-by-`n` Mandelbrot grid. *)
let mandelbrot n =
  let max_iter = 50 in
  let coord lo span p = lo +. (span *. float_of_int p /. float_of_int n) in
  let escape cx cy =
    let zx = ref 0.0 and zy = ref 0.0 in
    for _ = 1 to max_iter do
      let nzx = (!zx *. !zx) -. (!zy *. !zy) +. cx in
      let nzy = (2.0 *. !zx *. !zy) +. cy in
      zx := nzx;
      zy := nzy
    done;
    let m = (!zx *. !zx) +. (!zy *. !zy) in
    if m < 4.0 then m else 4.0
  in
  let acc = ref 0.0 in
  for j = 0 to n - 1 do
    let cy = coord (-1.25) 2.5 j in
    for i = 0 to n - 1 do
      acc := !acc +. escape (coord (-2.0) 2.5 i) cy
    done
  done;
  !acc

(* The Ackermann function at `m = 3`. *)
let ackermann n =
  let rec ack m n =
    if m = 0 then n + 1 else if n = 0 then ack (m - 1) 1 else ack (m - 1) (ack m (n - 1))
  in
  ack 3 n

(* The xor of `n` successive xorshift64 states (seeded constant). Uses `Int64` for
   the full 64-bit bit pattern; `>>` is the logical (unsigned) shift. *)
let prng_xorshift n =
  let open Int64 in
  let state = ref 88172645463325252L in
  let acc = ref 0L in
  for _ = 1 to n do
    state := logxor !state (shift_left !state 13);
    state := logxor !state (shift_right_logical !state 7);
    state := logxor !state (shift_left !state 17);
    acc := logxor !acc !state
  done;
  !acc

type token =
  | TNum of int
  | TPlus
  | TMinus
  | TStar

type expr =
  | ENum of int
  | EAdd of expr * expr
  | ESub of expr * expr
  | EMul of expr * expr

type eval_work = Visit of expr | AddResult | SubResult | MulResult

(* The value of a generated `n`-number arithmetic expression with `*` binding
   tighter than `+`/`-`. A token `list` is parsed by recursive descent into an
   `expr` tree (the linked structure a parser naturally consumes). *)
let expr_eval n =
  let num i = (i mod 9) + 1 in
  let op_for i = match i mod 3 with 0 -> TPlus | 1 -> TStar | _ -> TMinus in
  let gen_tokens count =
    let list = ref [] in
    let i = ref (count - 1) in
    while !i >= 0 do
      list := TNum (num !i) :: !list;
      if !i > 0 then list := op_for (!i - 1) :: !list;
      decr i
    done;
    !list
  in
  let parse_factor tokens =
    match tokens with TNum k :: rest -> Some (ENum k, rest) | _ -> None
  in
  let parse_term tokens =
    match parse_factor tokens with
    | None -> None
    | Some (left, rest) ->
      let rec loop left rest =
        match rest with
        | TStar :: more -> (
          match parse_factor more with
          | None -> None
          | Some (right, rest2) -> loop (EMul (left, right)) rest2)
        | _ -> Some (left, rest)
      in
      loop left rest
  in
  let parse_expr tokens =
    match parse_term tokens with
    | None -> None
    | Some (left, rest) ->
      let rec loop left rest =
        match rest with
        | TPlus :: more -> (
          match parse_term more with
          | None -> None
          | Some (right, rest2) -> loop (EAdd (left, right)) rest2)
        | TMinus :: more -> (
          match parse_term more with
          | None -> None
          | Some (right, rest2) -> loop (ESub (left, right)) rest2)
        | _ -> Some (left, rest)
      in
      loop left rest
  in
  let rec eval work values = match work, values with
    | [], result :: _ -> result
    | Visit (ENum k) :: rest, _ -> eval rest (k :: values)
    | Visit (EAdd (a, b)) :: rest, _ -> eval (Visit a :: Visit b :: AddResult :: rest) values
    | Visit (ESub (a, b)) :: rest, _ -> eval (Visit a :: Visit b :: SubResult :: rest) values
    | Visit (EMul (a, b)) :: rest, _ -> eval (Visit a :: Visit b :: MulResult :: rest) values
    | AddResult :: rest, right :: left :: tail -> eval rest ((left + right) :: tail)
    | SubResult :: rest, right :: left :: tail -> eval rest ((left - right) :: tail)
    | MulResult :: rest, right :: left :: tail -> eval rest ((left * right) :: tail)
    | _ -> assert false
  in
  match parse_expr (gen_tokens n) with Some (e, _) -> eval [Visit e] [] | None -> 0

(* Build and retain the same adjacency dictionary as the Fai workload. *)
let graph_adjacency n =
  let graph = Hashtbl.create 0 in
  for i = 0 to n - 1 do
    Hashtbl.add graph i [ (i + 1) mod n; ((2 * i) + 1) mod n; ((3 * i) + 2) mod n ]
  done;
  graph

let graph_reachable graph =
  let visited = Hashtbl.create 0 in
  Hashtbl.replace visited 0 ();
  let visit next node =
    if Hashtbl.mem visited node then next else begin
      Hashtbl.replace visited node ();
      node :: next
    end
  in
  let rec expand frontier next = match frontier with
    | [] -> next
    | node :: rest ->
      let neighbors = match Hashtbl.find_opt graph node with Some values -> values | None -> [] in
      let next = List.fold_left visit next neighbors in
      expand rest next
  in
  let rec bfs = function
    | [] -> Hashtbl.length visited
    | frontier -> bfs (expand frontier [])
  in
  bfs [0]

(* The number of nodes reachable from `0`, including adjacency construction. *)
let graph_bfs n = graph_reachable (graph_adjacency n)

(* The number of ways to make amount `n` from `[1,2,5,10,25,50]`, modulo a large
   prime (a dynamic program over a sub-amount table). *)
let coin_change n =
  let modulus = 1000000007 in
  let coins = [| 1; 2; 5; 10; 25; 50 |] in
  let ways = Array.make (n + 1) 0 in
  ways.(0) <- 1;
  Array.iter
    (fun coin ->
      for a = coin to n do
        ways.(a) <- (ways.(a) + ways.(a - coin)) mod modulus
      done)
    coins;
  ways.(n)

(* The `n`th Fibonacci number with two's-complement wrapping, computed top-down
   with a hash-map memo. Uses `Int64` to reproduce the oracle's i64 wrapping. *)
let fib_memo n =
  let memo : (int, Int64.t) Hashtbl.t = Hashtbl.create 0 in
  let rec go k =
    if k < 2 then Int64.of_int k
    else
      match Hashtbl.find_opt memo k with
      | Some v -> v
      | None ->
        let left = go (k - 1) in
        let right = go (k - 2) in
        let v = Int64.add left right in
        Hashtbl.replace memo k v;
        v
  in
  go n

(* The position-weighted checksum of a scrambled `n`-element `array` after sorting
   it ascending. *)
let quicksort_sum n =
  let v = Array.init n (fun k -> ((k * 2654435761) + 12345) mod n) in
  let swap i j = let x = v.(i) in v.(i) <- v.(j); v.(j) <- x in
  let rec sort low high =
    if high - low > 1 then begin
      let store = ref low in
      for index = low to high - 2 do
        if v.(index) < v.(high - 1) then begin swap !store index; incr store end
      done;
      swap !store (high - 1);
      if !store - low < high - !store - 1 then begin sort low !store; sort (!store + 1) high end
      else begin sort (!store + 1) high; sort low !store end
    end
  in
  sort 0 (Array.length v);
  let acc = ref 0 in
  Array.iteri (fun i x -> acc := !acc + (i * x)) v;
  !acc

(* The number of primes below `n` (Sieve of Eratosthenes). *)
let sieve n =
  if n < 2 then 0
  else begin
    let composite = Array.make n false in
    let p = ref 2 in
    while !p * !p < n do
      if not composite.(!p) then begin
        let m = ref (!p * !p) in
        while !m < n do
          composite.(!m) <- true;
          m := !m + !p
        done
      end;
      incr p
    done;
    let count = ref 0 in
    for i = 2 to n - 1 do
      if not composite.(i) then incr count
    done;
    !count
  end

(* The position checksum of five gravitating bodies after `n` all-pairs steps.
   The force accumulation and final sum fold in the same order as the Fai
   version. *)
let nbody n =
  let dt = 0.01 in
  let bodies =
    Array.init 5 (fun i ->
        [|
          float_of_int i;
          float_of_int (i * i mod 7);
          float_of_int (i mod 3);
          0.0;
          0.0;
          0.0;
          1.0 +. float_of_int (i mod 5);
        |])
  in
  for _ = 1 to n do
    let snapshot = Array.map Array.copy bodies in
    Array.iteri
      (fun me_idx b ->
        let me = snapshot.(me_idx) in
        let ax = ref 0.0 and ay = ref 0.0 and az = ref 0.0 in
        Array.iteri
          (fun o_idx o ->
            if o_idx <> me_idx then begin
              let dx = o.(0) -. me.(0) in
              let dy = o.(1) -. me.(1) in
              let dz = o.(2) -. me.(2) in
              let d2 = (dx *. dx) +. (dy *. dy) +. (dz *. dz) in
              let f = o.(6) /. (d2 *. sqrt d2) in
              ax := !ax +. (f *. dx);
              ay := !ay +. (f *. dy);
              az := !az +. (f *. dz)
            end)
          snapshot;
        b.(3) <- me.(3) +. (dt *. !ax);
        b.(4) <- me.(4) +. (dt *. !ay);
        b.(5) <- me.(5) +. (dt *. !az))
      bodies;
    Array.iter
      (fun b ->
        b.(0) <- b.(0) +. (dt *. b.(3));
        b.(1) <- b.(1) +. (dt *. b.(4));
        b.(2) <- b.(2) +. (dt *. b.(5)))
      bodies
  done;
  let acc = ref 0.0 in
  Array.iter (fun b -> acc := !acc +. b.(0) +. b.(1) +. b.(2)) bodies;
  !acc

(* The maximum pancake-flip count over every permutation of `[1, n]`
   (fannkuch-redux). Permutations and the permutation collection are `list`s,
   matching the Fai sample's linked structure. *)
let fannkuch n =
  let rec take k xs = match xs with h :: t when k > 0 -> h :: take (k - 1) t | _ -> [] in
  let rec drop k xs = match xs with _ :: t when k > 0 -> drop (k - 1) t | _ -> xs in
  let reverse_first k xs = List.rev (take k xs) @ drop k xs in
  let rec flips_from acc perm =
    match perm with
    | first :: _ when first > 1 -> flips_from (acc + 1) (reverse_first first perm)
    | _ -> acc
  in
  let rec remove_first y xs =
    match xs with [] -> [] | x :: rest -> if x = y then rest else x :: remove_first y rest
  in
  let rec perms xs =
    match xs with
    | [] -> [ [] ]
    | _ ->
      let groups = List.map (fun x -> List.map (fun p -> x :: p) (perms (remove_first x xs))) xs in
      List.concat groups
  in
  let rec range lo hi = if lo >= hi then [] else lo :: range (lo + 1) hi in
  List.fold_left (fun acc p -> max acc (flips_from 0 p)) 0 (perms (range 1 (n + 1)))

(* The number of connected components among `n` nodes after linking each `i` (not
   a multiple of 5) to `i/2` via union-find. *)
let union_find n =
  let parent = Hashtbl.create 0 in
  let rec find x =
    match Hashtbl.find_opt parent x with Some p when p <> x -> find p | _ -> x
  in
  for i = 1 to n - 1 do
    if i mod 5 <> 0 then begin
      let ra = find i in
      let rb = find (i / 2) in
      if ra <> rb then Hashtbl.replace parent ra rb
    end
  done;
  let roots = Hashtbl.create 0 in
  for i = 0 to n - 1 do
    Hashtbl.replace roots (find i) ()
  done;
  Hashtbl.length roots

type json =
  | JNull
  | JBool of bool
  | JInt of int
  | JArr of json list
  | JObj of (string * json) list

(* The length of the serialized balanced JSON tree of `n` nodes. *)
let json_text n =
  let leaf seed = match seed mod 3 with 0 -> JNull | 1 -> JBool true | _ -> JInt seed in
  let rec build seed size =
    if size <= 1 then leaf seed
    else begin
      let half = size / 2 in
      let l = build (seed + 1) half in
      let r = build (seed + 2) (size - half - 1) in
      if seed mod 2 = 0 then JArr [ l; r ] else JObj [ ("a", l); ("b", r) ]
    end
  in
  let tree = build 0 n in
  let buf = Buffer.create 0 in
  let rec ser j =
    match j with
    | JNull -> Buffer.add_string buf "null"
    | JBool b -> Buffer.add_string buf (if b then "true" else "false")
    | JInt k -> Buffer.add_string buf (string_of_int k)
    | JArr items ->
      Buffer.add_char buf '[';
      List.iteri
        (fun i it ->
          if i > 0 then Buffer.add_char buf ',';
          ser it)
        items;
      Buffer.add_char buf ']'
    | JObj fields ->
      Buffer.add_char buf '{';
      List.iteri
        (fun i (k, v) ->
          if i > 0 then Buffer.add_char buf ',';
          Buffer.add_char buf '"';
          Buffer.add_string buf k;
          Buffer.add_string buf "\":";
          ser v)
        fields;
      Buffer.add_char buf '}'
  in
  ser tree;
  Buffer.contents buf

let json_serialize n = unicode_length (json_text n)

(* The length of the string built by appending `"ab"` onto `"0"` `n` times. *)
let string_build_text n =
  let buf = Buffer.create 0 in
  Buffer.add_string buf (string_of_int 0);
  for _ = 1 to n do
    Buffer.add_string buf "ab"
  done;
  Buffer.contents buf

let string_build n = unicode_length (string_build_text n)

(* The total length of 200 half-length substrings of a length-`n` base. *)
let slice_base n =
  let buffer = Buffer.create 0 in
  for _ = 1 to n do Buffer.add_char buffer 'a' done;
  Buffer.contents buffer

let unicode_prefix count text =
  let offset = ref 0 and remaining = ref count in
  while !remaining > 0 && !offset < String.length text do
    let first = Char.code text.[!offset] in
    offset := !offset + (if first < 0x80 then 1 else if first < 0xe0 then 2 else if first < 0xf0 then 3 else 4);
    decr remaining
  done;
  String.sub text 0 !offset

let string_slice n =
  let base = slice_base n in
  let half = max (n / 2) 0 in
  let prefixes = Array.init 200 (fun _ -> unicode_prefix half base) in
  array_string_lengths prefixes

(* Sum the safe-evaluation results over `[0, n)`, taking the chain at `i` or, when
   it fails (a zero divisor), the one at `i + 1`. *)
let option_eval n =
  let safe_div a b = if b = 0 then None else Some (a / b) in
  let eval_chain i =
    match safe_div (i * i) (i mod 3) with
    | None -> None
    | Some x -> ( match safe_div x (i mod 4) with None -> None | Some y -> safe_div (x + y) (i mod 5))
  in
  let acc = ref 0 in
  for i = 0 to n - 1 do
    let first = eval_chain i in
    let second = eval_chain (i + 1) in
    let r = match first with Some _ as s -> s | None -> second in
    match r with Some v -> acc := !acc + v | None -> ()
  done;
  !acc

(* The `Int`-only twin of `option_eval`: -1 marks failure instead of `None`. *)
let int_eval n =
  let safe_div a b = if b = 0 then -1 else a / b in
  let eval_chain i =
    let x = safe_div (i * i) (i mod 3) in
    if x = -1 then -1
    else begin
      let y = safe_div x (i mod 4) in
      if y = -1 then -1 else safe_div (x + y) (i mod 5)
    end
  in
  let acc = ref 0 in
  for i = 0 to n - 1 do
    let first = eval_chain i in
    let second = eval_chain (i + 1) in
    let v = if first = -1 then second else first in
    if v <> -1 then acc := !acc + v
  done;
  !acc

(* Follow "next pointer" chains through a lookup table, summing visited keys; a
   missing key ends the walk, and a per-walk fuel bounds the cyclic table. *)
let option_path n =
  let size = 100 in
  let table = Hashtbl.create 0 in
  for i = 0 to size - 1 do
    Hashtbl.replace table i (((i * 2) + 1) mod size)
  done;
  let total = ref 0 in
  for i = 0 to n - 1 do
    let key = ref (i mod (size * 2)) in
    let fuel = ref size in
    let acc = ref 0 in
    let go = ref true in
    while !go && !fuel > 0 do
      match Hashtbl.find_opt table !key with
      | None -> go := false
      | Some next ->
        acc := !acc + !key;
        key := next;
        decr fuel
    done;
    total := !total + !acc
  done;
  !total

type lookup_tree = LLeaf | LNode of { mutable left : lookup_tree; key : int; mutable value : int; mutable right : lookup_tree }

let rec lookup_insert key value = function
  | LLeaf -> LNode { left = LLeaf; key; value; right = LLeaf }
  | LNode node as tree ->
    if key < node.key then node.left <- lookup_insert key value node.left
    else if key > node.key then node.right <- lookup_insert key value node.right
    else node.value <- value;
    tree

let rec lookup_build lo hi tree =
  if lo >= hi then tree else
  let mid = (lo + hi) / 2 in
  let tree = lookup_insert mid (mid * 3) tree in
  let tree = lookup_build lo mid tree in
  lookup_build (mid + 1) hi tree

let rec lookup_find key = function
  | LLeaf -> None
  | LNode node ->
    if key < node.key then lookup_find key node.left else if key > node.key then lookup_find key node.right else Some node.value

(* Midpoint-first binary nodes with borrowed lookup and optional results. *)
let option_tree_find n =
  let m = 1000 in
  let tree = lookup_build 0 m LLeaf in
  let total = ref 0 in
  for i = 0 to n - 1 do
    match lookup_find (i mod (m * 2)) tree with Some v -> total := !total + v | None -> ()
  done;
  !total

(* The position-weighted checksum of a scrambled `n`-element `list` after sorting
   it — the linked counterpart to `merge_sort_sum`'s array sort. *)
let linked_sort cmp values =
  let rec merge left right = match left, right with
    | [], _ -> right
    | _, [] -> left
    | a :: rest_a, b :: rest_b ->
      if cmp a b <= 0 then a :: merge rest_a right else b :: merge left rest_b
  in
  let rec pairs = function
    | [] -> []
    | [run] -> [run]
    | a :: b :: rest -> let run = merge a b in run :: pairs rest
  in
  let rec all = function [] -> [] | [run] -> run | runs -> all (pairs runs) in
  let n = List.length values in
  if n <= 32 then all (List.map (fun value -> [value]) values)
  else let source = Array.of_list values in
    Array.to_list (merge_buffers cmp source (Array.copy source))

let list_sort_sum n =
  let sorted = linked_sort (fun a b -> a - b) (Array.to_list (sort_input 2 n)) in
  let acc = ref 0 in
  List.iteri (fun i x -> acc := !acc + (i * x)) sorted;
  !acc

let sort_pattern pattern n =
  let n = max n 0 in
  let values = Array.init n (fun i ->
    match pattern with
    | 1 -> n - 1 - i
    | 3 -> 7
    | 4 -> i mod 4
    | 5 -> n - min n ((i / 32 + 1) * 32) + i mod 32
    | _ -> i) in
  if pattern = 2 then begin
    let state = ref 1 in
    for i = n - 1 downto 1 do
      state := (!state * 1664525 + 1013904223) land 2147483647;
      let j = !state mod (i + 1) in
      let x = values.(i) in values.(i) <- values.(j); values.(j) <- x
    done
  end;
  Array.sort compare values;
  let answer = ref 0 in
  Array.iteri (fun i x -> answer := !answer + i * x) values;
  !answer

type workload = WInt of (int -> int) | WInt64 of (int -> Int64.t) | WFloat of (int -> float)

let workload = function
  | "Fib" -> WInt fib
  | "Collatz" -> WInt collatz_sum
  | "MapSum" -> WInt map_sum
  | "MergeSort" -> WInt merge_sort_sum
  | "SortAscending" -> WInt (sort_pattern 0)
  | "SortDescending" -> WInt (sort_pattern 1)
  | "SortShuffled" -> WInt (sort_pattern 2)
  | "SortEqual" -> WInt (sort_pattern 3)
  | "SortFewKeys" -> WInt (sort_pattern 4)
  | "SortRuns" -> WInt (sort_pattern 5)
  | "BinaryTrees" -> WInt tree_count
  | "Pi" -> WFloat pi
  | "DictHistogram" -> WInt dict_histogram
  | "WordCount" -> WInt word_count
  | "MapSumShared" -> WInt map_sum_shared
  | "SetDedup" -> WInt set_dedup
  | "FoldPipeline" -> WInt fold_pipeline
  | "InterfaceDispatch" -> WInt interface_dispatch
  | "Particles" -> WFloat particles
  | "VecMat" -> WFloat vec_mat
  | "NQueens" -> WInt nqueens
  | "MatrixMultiply" -> WInt matrix_multiply
  | "FloatMatrixMultiply" -> WFloat float_matrix_multiply
  | "Levenshtein" -> WInt levenshtein
  | "GameOfLife" -> WInt game_of_life
  | "SpectralNorm" -> WFloat spectral_norm
  | "Mandelbrot" -> WFloat mandelbrot
  | "Ackermann" -> WInt ackermann
  | "PrngXorshift" -> WInt64 prng_xorshift
  | "ExprEval" -> WInt expr_eval
  | "GraphBFS" -> WInt graph_bfs
  | "CoinChange" -> WInt coin_change
  | "FibMemo" -> WInt64 fib_memo
  | "QuickSort" -> WInt quicksort_sum
  | "Sieve" -> WInt sieve
  | "NBody" -> WFloat nbody
  | "Fannkuch" -> WInt fannkuch
  | "UnionFind" -> WInt union_find
  | "JsonSerialize" -> WInt json_serialize
  | "StringBuild" -> WInt string_build
  | "StringSlice" -> WInt string_slice
  | "OptionEval" -> WInt option_eval
  | "IntEval" -> WInt int_eval
  | "OptionPath" -> WInt option_path
  | "OptionTreeFind" -> WInt option_tree_find
  | "ListSort" -> WInt list_sort_sum
  | other -> failwith ("unknown algorithm module: " ^ other)

let evaluate selected n =
  match selected with WInt f -> I (f n) | WInt64 f -> I64 (f n) | WFloat f -> F (f n)

let () =
  let result = evaluate (workload Sys.argv.(1)) (int_of_string Sys.argv.(2)) in
  (match result with
  | I v -> Printf.printf "%d\n" v
  | I64 v -> Printf.printf "%Ld\n" v
  | F v -> Printf.printf "%.17g\n" v);
  if Sys.getenv_opt "FAI_REPORT_RSS" <> None then
    match peak_rss_kib () with Some kib -> Printf.eprintf "fai-peak-rss-kib: %d\n" kib | None -> ()
