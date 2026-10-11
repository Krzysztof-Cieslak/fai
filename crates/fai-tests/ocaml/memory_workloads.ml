(* Whole-structure consumers retain materialization without element barriers. *)
let[@inline never] memory_sum_int values =
  let total = ref (Array.length values) in
  Array.iter (fun value -> total := !total + value) values;
  !total

let memory_array_int n = memory_sum_int (Array.init n Fun.id)

let[@inline never] memory_sum_float values =
  let total = ref 0.0 in
  Array.iter (fun value -> total := !total +. value) values;
  !total +. float_of_int (Array.length values)

let memory_array_float n = memory_sum_float (Array.init n (fun i -> float_of_int i +. 0.25))

let memory_shared_arrays n =
  let original = Array.init n Fun.id in
  let changed = Array.copy original in
  for i = 0 to n - 1 do changed.(i) <- changed.(i) + 1 done;
  memory_sum_int original + memory_sum_int changed

let[@inline never] memory_sum_list values =
  let rec loop total = function [] -> total | value :: rest -> loop (total + value + 1) rest in
  loop 0 values

let memory_list n =
  let rec build i values = if i >= n then values else build (i + 1) (i :: values) in
  memory_sum_list (build 0 [])

type memory_tree_value = MemoryEmpty | MemoryNode of int * memory_tree_value * memory_tree_value

let rec memory_tree_make depth =
  if depth <= 0 then MemoryEmpty
  else MemoryNode (depth, memory_tree_make (depth - 1), memory_tree_make (depth - 1))

let[@inline never] rec memory_tree_sum = function
  | MemoryEmpty -> 0
  | MemoryNode (value, left, right) -> value + 1 + memory_tree_sum left + memory_tree_sum right

let memory_tree depth = memory_tree_sum (memory_tree_make depth)

let[@inline never] memory_utf8_length text =
  let count = ref 0 in
  String.iter (fun c -> if Char.code c land 0xc0 <> 0x80 then incr count) text;
  !count

let memory_utf8 n =
  let buffer = Buffer.create 0 in
  for _ = 1 to n do Buffer.add_string buffer "aéλ😀" done;
  memory_utf8_length (Buffer.contents buffer)

let memory_histogram n =
  if n <= 0 then 0 else
  let table = Hashtbl.create 0 in
  for i = 0 to n * 4 - 1 do
    let key = i mod n in
    let value = Option.value (Hashtbl.find_opt table key) ~default:0 in
    Hashtbl.replace table key (value + 1)
  done;
  Hashtbl.length table + Hashtbl.find table 0 + Hashtbl.find table (n - 1)

let memory_set n =
  if n <= 0 then 0 else
  let table = Hashtbl.create 0 in
  for i = 0 to n * 4 - 1 do Hashtbl.replace table (i mod n) () done;
  Hashtbl.length table + Bool.to_int (Hashtbl.mem table 0) + Bool.to_int (Hashtbl.mem table (n - 1))

let[@inline never] memory_graph_walk graph =
  let n = Array.length graph in
  if n = 0 then 0 else
  let visited = Array.make n false in
  let queue = ref (Array.make 1 0) in
  let length = ref 1 in
  let push value =
    if !length = Array.length !queue then begin
      let next = Array.make (!length * 2) 0 in
      Array.blit !queue 0 next 0 !length;
      queue := next
    end;
    (!queue).(!length) <- value;
    incr length
  in
  visited.(0) <- true;
  let cursor = ref 0 in
  while !cursor < !length do
    let node = (!queue).(!cursor) in
    Array.iter (fun next -> if not visited.(next) then begin visited.(next) <- true; push next end) graph.(node);
    incr cursor
  done;
  !cursor

let memory_graph n =
  if n <= 0 then 0 else
  memory_graph_walk (Array.init n (fun i -> [| (i + 1) mod n; (i * 2 + 1) mod n; (i * 3 + 2) mod n |]))

type memory_body = { x : float; y : float; vx : float; vy : float }

let[@inline never] memory_body_sum values =
  Array.fold_left (fun total body -> total +. body.x +. body.y) 0.0 values

let memory_bodies n =
  let values = ref (Array.init n (fun i -> { x = float_of_int i; y = 0.0; vx = 0.25; vy = 0.5 })) in
  for _ = 1 to 4 do
    values := Array.map (fun body -> { body with x = body.x +. body.vx; y = body.y +. body.vy }) !values
  done;
  memory_body_sum !values
