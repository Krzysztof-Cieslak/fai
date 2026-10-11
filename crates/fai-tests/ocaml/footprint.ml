(* The workload frame returns before the parent observes post-workload RSS.
   No forced collection changes the runtime's normal reclamation policy. *)
let[@inline never] footprint_execute size count mode =
  let total = ref HARNESS_ZERO in
  for _ = 1 to count do
    let value = if mode = "floor" then HARNESS_FLOOR else HARNESS_VALUE in
    total := HARNESS_ADD
  done;
  Printf.printf "HARNESS_FORMAT\n" !total;
  flush stdout

let () =
  if Array.length Sys.argv <> 4 then invalid_arg "expected size, repetitions, and run/floor";
  let size = int_of_string Sys.argv.(1) in
  let count = int_of_string Sys.argv.(2) in
  let mode = Sys.argv.(3) in
  if size < 0 || size > 2147483647 || count < 0 || count > 1048576 || (mode <> "run" && mode <> "floor") then invalid_arg "memory arguments out of range";
  if read_line () <> "prepare" then invalid_arg "expected prepare";
  print_endline "ready";
  if read_line () <> "run" then invalid_arg "expected run";
  footprint_execute size count mode;
  match input_line stdin with
  | _ -> invalid_arg "expected EOF after footprint result"
  | exception End_of_file -> ()
