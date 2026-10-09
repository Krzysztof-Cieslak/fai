(* Four logical fields per binary node. Mutation is confined to the unique
   midpoint-first build; lookup holds and reads the completed tree. *)
type tree =
  | Leaf
  | Node of { mutable left : tree; key : int; mutable value : int; mutable right : tree }

let rec insert key value = function
  | Leaf -> Node { left = Leaf; key; value; right = Leaf }
  | Node node as tree ->
    if key < node.key then node.left <- insert key value node.left
    else if key > node.key then node.right <- insert key value node.right
    else node.value <- value;
    tree

let rec build_range lo hi tree =
  if lo >= hi then tree
  else
    let mid = lo + (hi - lo) / 2 in
    let tree = insert mid (mid * 3) tree in
    let tree = build_range lo mid tree in
    build_range (mid + 1) hi tree

let build n = build_range 0 n Leaf

let rec find key = function
  | Leaf -> None
  | Node node ->
    if key < node.key then find key node.left
    else if key > node.key then find key node.right
    else Some node.value

let rec shape = function
  | Leaf -> [-1]
  | Node n -> n.key :: n.value :: (shape n.left @ shape n.right)

let rec dimensions = function
  | Leaf -> (0, 0)
  | Node n ->
    let lc, lh = dimensions n.left in
    let rc, rh = dimensions n.right in
    (lc + rc + 1, 1 + max lh rh)

let checksum find n =
  let total = ref 0 in
  for i = 0 to n - 1 do
    let value = match find (i mod 2000) with None -> -1 | Some v -> v in
    total := !total + (i + 1) * value
  done;
  !total

module IntMap = Map.Make (Int)
let build_map n =
  let result = ref IntMap.empty in
  for k = 0 to n - 1 do result := IntMap.add k (k * 3) !result done;
  !result

let () =
  let mode = Sys.argv.(1) in
  if mode = "describe" then begin
    let count = int_of_string Sys.argv.(2) in
    let tree = build count in
    let nodes, height = dimensions tree in
    Printf.printf "%d,%d\n" nodes height;
    print_endline (String.concat "," (List.map string_of_int (shape tree)));
    print_endline (String.concat "," (List.init (count * 2 + 2) (fun i ->
      match find (i - 1) tree with None -> "none" | Some v -> string_of_int v)))
  end else begin
    let rebuild = Sys.argv.(2) = "build" in
    let binary = mode = "binary" in
    let tree = if binary && not rebuild then build 1000 else Leaf in
    let map = if not binary && not rebuild then build_map 1000 else IntMap.empty in
    print_endline "ready";
    flush stdout;
    try while true do
      let n = int_of_string (read_line ()) in
      let answer = if binary then
        let current = if rebuild then build 1000 else tree in
        checksum (fun key -> find key current) n
      else
        let current = if rebuild then build_map 1000 else map in
        checksum (fun key -> IntMap.find_opt key current) n in
      print_endline (string_of_int answer);
      flush stdout
    done with End_of_file -> ()
  end
