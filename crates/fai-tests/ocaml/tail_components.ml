(* Supplemental matched workloads. String indices and lengths count Unicode scalars. *)
let ascii n =
  let text = Buffer.create 0 in
  for _ = 1 to n do Buffer.add_char text 'a' done;
  Buffer.contents text

let unicode n =
  let text = Buffer.create 0 in
  for _ = 1 to n do Buffer.add_string text "aéλ😀" done;
  Buffer.contents text

let length text =
  let count = ref 0 in
  for i = 0 to String.length text - 1 do
    if Char.code text.[i] land 0xc0 <> 0x80 then incr count
  done;
  !count

let prefix count text =
  let index = ref 0 and remaining = ref count in
  while !remaining > 0 && !index < String.length text do
    let first = Char.code text.[!index] in
    index := !index + (if first < 0x80 then 1 else if first < 0xe0 then 2 else if first < 0xf0 then 3 else 4);
    decr remaining
  done;
  String.sub text 0 !index

let lengths text =
  let total = ref 0 in
  for _ = 1 to 200 do total := !total + length text done;
  !total

let views text =
  let half = length text / 2 in
  let prefixes = Array.init 200 (fun i -> prefix (half + i mod 3) text) in
  Array.fold_left (fun total text -> total + length text) 0 prefixes

let quicksort n =
  let values = Array.init (max 0 n) (fun k -> (k * 2654435761 + 12345) mod n) in
  let swap i j = let x = values.(i) in values.(i) <- values.(j); values.(j) <- x in
  let rec sort low high =
    if high - low > 1 then begin
      let store = ref low in
      for index = low to high - 2 do
        if values.(index) < values.(high - 1) then begin
          swap !store index;
          incr store
        end
      done;
      swap !store (high - 1);
      if !store - low < high - !store - 1 then begin
        sort low !store; sort (!store + 1) high
      end else begin
        sort (!store + 1) high; sort low !store
      end
    end
  in
  sort 0 (Array.length values);
  let total = ref 0 in
  Array.iteri (fun i value -> total := !total + i * value) values;
  !total

type worker = WInt of (int -> int) | WInt64 of (int -> int64) | WFloat of (int -> float)
let workload = function
  | "TailBuildAscii" -> WInt (fun n -> length (ascii n))
  | "TailBuildUnicode" -> WInt (fun n -> length (unicode n))
  | "TailLengthAscii" -> WInt (fun n -> lengths (ascii n))
  | "TailLengthUnicode" -> WInt (fun n -> lengths (unicode n))
  | "TailViewsAscii" -> WInt (fun n -> views (ascii n))
  | "TailViewsUnicode" -> WInt (fun n -> views (unicode n))
  | "TailQuickSort" -> WInt quicksort
  | name -> invalid_arg name
