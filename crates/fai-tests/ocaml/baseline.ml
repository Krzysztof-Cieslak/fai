(* The OCaml side of the subprocess runtime/memory comparison: run one algorithm
   at one size and print the result.

   This is the OCaml twin of `src/bin/algo-baseline.rs`. The benchmark spawns it
   against the `fai build` binary (and the Rust release binary) so it compares a
   delivered, ocamlopt-compiled native executable to a delivered Fai binary
   (process startup, the workload, and exit). Printing the result keeps the
   computation from being optimized away, matching the other binaries' `main`.

   Each implementation matches its Fai sample's / the Rust oracle's data
   representation, so the comparison measures the runtime/codegen gap rather than
   an incidental data-structure difference: a workload that iterates or indexes
   uses a contiguous `array`, and one that is naturally persistent — backtracking,
   or a cons-pattern-matched parser — uses an OCaml `list` (the faithful twin of
   Fai's linked `List`). Hash workloads use `Hashtbl`, ordered ones `Map`/`Set`.

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

(* Sum of doubling every element of `[0, n)`. `Sys.opaque_identity` per element
   keeps the loop from collapsing to a closed form, matching the Rust oracle's
   `black_box`. *)
let map_sum n =
  let acc = ref 0 in
  for x = 0 to n - 1 do
    acc := !acc + Sys.opaque_identity (x * 2)
  done;
  !acc

(* Sum of `[n-1, …, 0]` after sorting it ascending (an `array`, like the Fai
   sample's `Array` and the Rust `Vec`). *)
let merge_sort_sum n =
  let v = Array.init n (fun i -> n - 1 - i) in
  Array.sort compare v;
  Array.fold_left ( + ) 0 v

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
  let counts = Hashtbl.create 512 in
  for i = 0 to n - 1 do
    let k = i mod buckets in
    let c = try Hashtbl.find counts k with Not_found -> 0 in
    Hashtbl.replace counts k (c + 1)
  done;
  Hashtbl.fold (fun key count acc -> acc + (key * count)) counts 0

(* The total length of the words in `"0 1 2 … n-1"` after joining with spaces and
   splitting back apart. *)
let word_count n =
  let buf = Buffer.create ((n * 4) + 1) in
  for i = 0 to n - 1 do
    if i > 0 then Buffer.add_char buf ' ';
    Buffer.add_string buf (string_of_int i)
  done;
  let parts = String.split_on_char ' ' (Buffer.contents buf) in
  List.fold_left (fun acc w -> acc + String.length w) 0 parts

(* The shared-list twin of `map_sum`: the sum of `2x` plus the sum of `x`. *)
let map_sum_shared n =
  let acc = ref 0 in
  for x = 0 to n - 1 do
    acc := !acc + Sys.opaque_identity (x * 2);
    acc := !acc + Sys.opaque_identity x
  done;
  !acc

(* The sum of the distinct values among `[0, n)` reduced modulo a bucket count. *)
let set_dedup n =
  let buckets = 1000 in
  let s = Hashtbl.create 2048 in
  for i = 0 to n - 1 do
    Hashtbl.replace s (i mod buckets) ()
  done;
  Hashtbl.fold (fun k () acc -> acc + k) s 0

(* The sum over `[0, n)` of `((x + 1) * 2) + 3`. *)
let fold_pipeline n =
  let acc = ref 0 in
  for x = 0 to n - 1 do
    acc := !acc + Sys.opaque_identity (((x + 1) * 2) + 3)
  done;
  !acc

(* The dispatched-score sum over `[0, n)`: `i % 3` selects `2i`, `i+1`, or `-i`. *)
let interface_dispatch n =
  let acc = ref 0 in
  for i = 0 to n - 1 do
    let v = match i mod 3 with 0 -> i * 2 | 1 -> i + 1 | _ -> -i in
    acc := !acc + v
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

(* The edit distance between two length-`n` integer sequences by a two-row DP. *)
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
  let live = ref (Hashtbl.create 256) in
  List.iter (fun cell -> Hashtbl.replace !live cell ()) [ (1, 0); (2, 0); (0, 1); (1, 1); (1, 2) ];
  for _ = 1 to n do
    let counts = Hashtbl.create 1024 in
    Hashtbl.iter
      (fun (x, y) () ->
        Array.iter
          (fun (dx, dy) ->
            let key = (x + dx, y + dy) in
            let c = try Hashtbl.find counts key with Not_found -> 0 in
            Hashtbl.replace counts key (c + 1))
          offsets)
      !live;
    let next = Hashtbl.create 1024 in
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
  let rec eval = function
    | ENum k -> k
    | EAdd (a, b) -> eval a + eval b
    | ESub (a, b) -> eval a - eval b
    | EMul (a, b) -> eval a * eval b
  in
  match parse_expr (gen_tokens n) with Some (e, _) -> eval e | None -> 0

(* The number of nodes reachable from `0` in the deterministic `n`-node graph. *)
let graph_bfs n =
  let neighbors i = [ (i + 1) mod n; ((2 * i) + 1) mod n; ((3 * i) + 2) mod n ] in
  let visited = Hashtbl.create 1024 in
  Hashtbl.replace visited 0 ();
  let frontier = ref [ 0 ] in
  while !frontier <> [] do
    let next = ref [] in
    List.iter
      (fun node ->
        List.iter
          (fun nb ->
            if not (Hashtbl.mem visited nb) then begin
              Hashtbl.replace visited nb ();
              next := nb :: !next
            end)
          (neighbors node))
      !frontier;
    frontier := !next
  done;
  Hashtbl.length visited

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
  let memo : (int, Int64.t) Hashtbl.t = Hashtbl.create 1024 in
  let rec go k =
    if k < 2 then Int64.of_int k
    else
      match Hashtbl.find_opt memo k with
      | Some v -> v
      | None ->
        let v = Int64.add (go (k - 1)) (go (k - 2)) in
        Hashtbl.replace memo k v;
        v
  in
  go n

(* The position-weighted checksum of a scrambled `n`-element `array` after sorting
   it ascending. *)
let quicksort_sum n =
  let v = Array.init n (fun k -> ((k * 2654435761) + 12345) mod n) in
  Array.sort compare v;
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
      List.fold_right
        (fun x acc -> List.map (fun p -> x :: p) (perms (remove_first x xs)) @ acc)
        xs []
  in
  let rec range lo hi = if lo >= hi then [] else lo :: range (lo + 1) hi in
  List.fold_left (fun acc p -> max acc (flips_from 0 p)) 0 (perms (range 1 (n + 1)))

(* The number of connected components among `n` nodes after linking each `i` (not
   a multiple of 5) to `i/2` via union-find. *)
let union_find n =
  let parent = Hashtbl.create 1024 in
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
  let roots = Hashtbl.create 1024 in
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
let json_serialize n =
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
  let buf = Buffer.create 1024 in
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
  ser (build 0 n);
  String.length (Buffer.contents buf)

(* The length of the string built by appending `"ab"` onto `"0"` `n` times. *)
let string_build n =
  let buf = Buffer.create ((2 * n) + 1) in
  Buffer.add_string buf (string_of_int 0);
  for _ = 1 to n do
    Buffer.add_string buf "ab"
  done;
  String.length (Buffer.contents buf)

(* The total length of 200 half-length substrings of a length-`n` base. *)
let string_slice n =
  let base = String.make (max n 0) 'a' in
  let half = max (n / 2) 0 in
  let acc = ref 0 in
  for _ = 1 to 200 do
    acc := !acc + String.length (String.sub base 0 half)
  done;
  !acc

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
    let r = match eval_chain i with Some _ as s -> s | None -> eval_chain (i + 1) in
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
    let v = if first = -1 then eval_chain (i + 1) else first in
    if v <> -1 then acc := !acc + v
  done;
  !acc

(* Follow "next pointer" chains through a lookup table, summing visited keys; a
   missing key ends the walk, and a per-walk fuel bounds the cyclic table. *)
let option_path n =
  let size = 100 in
  let table = Hashtbl.create 256 in
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

module IntMap = Map.Make (Int)

(* Balanced-tree `find`, summing the hits; queries miss half the time. *)
let option_tree_find n =
  let m = 1000 in
  let tree = ref IntMap.empty in
  for k = 0 to m - 1 do
    tree := IntMap.add k (k * 3) !tree
  done;
  let total = ref 0 in
  for i = 0 to n - 1 do
    match IntMap.find_opt (i mod (m * 2)) !tree with Some v -> total := !total + v | None -> ()
  done;
  !total

(* The position-weighted checksum of a scrambled `n`-element `list` after sorting
   it — the linked counterpart to `merge_sort_sum`'s array sort. *)
let list_sort_sum n =
  let rec go k acc = if k < 0 then acc else go (k - 1) ((((k * 2654435761) + 12345) mod n) :: acc) in
  let sorted = List.sort compare (go (n - 1) []) in
  let acc = ref 0 in
  List.iteri (fun i x -> acc := !acc + (i * x)) sorted;
  !acc

let () =
  let module_name = Sys.argv.(1) in
  let n = int_of_string Sys.argv.(2) in
  let result =
    match module_name with
    | "Fib" -> I (fib n)
    | "Collatz" -> I (collatz_sum n)
    | "MapSum" -> I (map_sum n)
    | "MergeSort" -> I (merge_sort_sum n)
    | "BinaryTrees" -> I (tree_count n)
    | "Pi" -> F (pi n)
    | "DictHistogram" -> I (dict_histogram n)
    | "WordCount" -> I (word_count n)
    | "MapSumShared" -> I (map_sum_shared n)
    | "SetDedup" -> I (set_dedup n)
    | "FoldPipeline" -> I (fold_pipeline n)
    | "InterfaceDispatch" -> I (interface_dispatch n)
    | "Particles" -> F (particles n)
    | "VecMat" -> F (vec_mat n)
    | "NQueens" -> I (nqueens n)
    | "MatrixMultiply" -> I (matrix_multiply n)
    | "FloatMatrixMultiply" -> F (float_matrix_multiply n)
    | "Levenshtein" -> I (levenshtein n)
    | "GameOfLife" -> I (game_of_life n)
    | "SpectralNorm" -> F (spectral_norm n)
    | "Mandelbrot" -> F (mandelbrot n)
    | "Ackermann" -> I (ackermann n)
    | "PrngXorshift" -> I64 (prng_xorshift n)
    | "ExprEval" -> I (expr_eval n)
    | "GraphBFS" -> I (graph_bfs n)
    | "CoinChange" -> I (coin_change n)
    | "FibMemo" -> I64 (fib_memo n)
    | "QuickSort" -> I (quicksort_sum n)
    | "Sieve" -> I (sieve n)
    | "NBody" -> F (nbody n)
    | "Fannkuch" -> I (fannkuch n)
    | "UnionFind" -> I (union_find n)
    | "JsonSerialize" -> I (json_serialize n)
    | "StringBuild" -> I (string_build n)
    | "StringSlice" -> I (string_slice n)
    | "OptionEval" -> I (option_eval n)
    | "IntEval" -> I (int_eval n)
    | "OptionPath" -> I (option_path n)
    | "OptionTreeFind" -> I (option_tree_find n)
    | "ListSort" -> I (list_sort_sum n)
    | other -> failwith ("unknown algorithm module: " ^ other)
  in
  (match result with
  | I v -> Printf.printf "%d\n" v
  | I64 v -> Printf.printf "%Ld\n" v
  | F v -> Printf.printf "%.17g\n" v);
  if Sys.getenv_opt "FAI_REPORT_RSS" <> None then
    match peak_rss_kib () with Some kib -> Printf.eprintf "fai-peak-rss-kib: %d\n" kib | None -> ()
