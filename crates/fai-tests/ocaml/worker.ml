(* The native worker uses a statically selected workload. Configuration and
   requests arrive after process startup; only checksums cross the batch boundary. *)
let fail message =
  print_endline ("error: " ^ message);
  flush stdout;
  exit 2

let words line =
  String.split_on_char ' ' (String.trim line) |> List.filter (fun word -> word <> "")

let decimal word =
  let length = String.length word in
  let start = if length > 0 && (word.[0] = '+' || word.[0] = '-') then 1 else 0 in
  let valid = ref (length > start) in
  for i = start to length - 1 do
    if word.[i] < '0' || word.[i] > '9' then valid := false
  done;
  if !valid then int_of_string_opt word else None

let modulus = 1000000007
let canonical value = let n = value mod modulus in if n < 0 then n + modulus else n
let canonical64 value =
  let n = Int64.to_int (Int64.rem value 1000000007L) in
  if n < 0 then n + modulus else n

let fold_int inputs count f =
  let total = ref 0 and index = ref 0 in
  for _ = 1 to count do
    total := (!total + f inputs.(!index)) mod modulus;
    index := if !index + 1 = Array.length inputs then 0 else !index + 1
  done;
  !total

let fold_float inputs count f =
  let total = ref 0.0 and index = ref 0 in
  for _ = 1 to count do
    total := !total +. f inputs.(!index);
    index := if !index + 1 = Array.length inputs then 0 else !index + 1
  done;
  !total

let serve selected =
  let inputs =
    try
      let fields = words (read_line ()) in
      if fields = [] || List.length fields > 64 then fail "inputs";
      fields |> List.map (fun field -> match decimal field with
        | Some value when value >= 0 && value <= 2147483647 -> value
        | _ -> fail "inputs") |> Array.of_list
    with End_of_file -> fail "inputs"
  in
  print_endline "ready";
  flush stdout;
  try while true do
    let mode, count = match words (read_line ()) with
      | [mode; number] -> (match decimal number with Some n -> mode, n | None -> fail "request")
      | _ -> fail "request"
    in
    if mode = "value" && count >= 0 && count < Array.length inputs then begin
      match selected with
      | WInt f -> Printf.printf "%d\n" (f inputs.(count))
      | WInt64 f -> Printf.printf "%Ld\n" (f inputs.(count))
      | WFloat f -> Printf.printf "%.17g\n" (f inputs.(count))
    end else if (mode = "run" || mode = "floor") && count >= 0 && count <= 1048576 then begin
      match selected with
      | WInt f -> Printf.printf "%d\n" (fold_int inputs count (if mode = "floor" then canonical else fun n -> canonical (f n)))
      | WInt64 f -> Printf.printf "%d\n" (fold_int inputs count (if mode = "floor" then canonical else fun n -> canonical64 (f n)))
      | WFloat f -> Printf.printf "%.17g\n" (fold_float inputs count (if mode = "floor" then float_of_int else f))
    end else fail "request";
    flush stdout
  done with End_of_file -> ()

let () = serve (workload "HARNESS_MODULE")
