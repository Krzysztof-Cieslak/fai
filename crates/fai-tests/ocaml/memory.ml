(* Repeated-workload footprint; GC counters retain their own accounting domain. *)
let () =
  if Array.length Sys.argv <> 4 then invalid_arg "expected size, repetitions, and run/floor";
  let size = int_of_string Sys.argv.(1) in
  let count = int_of_string Sys.argv.(2) in
  let mode = Sys.argv.(3) in
  if size < 0 || size > 2147483647 || count < 0 || count > 1048576 || (mode <> "run" && mode <> "floor") then invalid_arg "memory arguments out of range";
  let before = if Sys.getenv_opt "FAI_REPORT_ALLOCATIONS" = None then None else Some (Gc.quick_stat ()) in
  let total = ref HARNESS_ZERO in
  for _ = 1 to count do
    let value = if mode = "floor" then HARNESS_FLOOR else HARNESS_VALUE in
    total := HARNESS_ADD
  done;
  (match before with
  | None -> ()
  | Some before ->
    let after = Gc.quick_stat () in
    let words s = s.Gc.minor_words +. s.Gc.major_words -. s.Gc.promoted_words in
    Printf.eprintf "ocaml-allocation-stats: {\"domain\":\"ocaml-gc-v1\",\"allocatedBytes\":%.0f,\"minorCollections\":%d,\"majorCollections\":%d,\"majorHeapWords\":%d,\"peakMajorHeapWords\":%d}\n"
      ((words after -. words before) *. float_of_int (Sys.word_size / 8))
      (after.Gc.minor_collections - before.Gc.minor_collections)
      (after.Gc.major_collections - before.Gc.major_collections)
      after.Gc.heap_words after.Gc.top_heap_words);
  Printf.printf "HARNESS_FORMAT\n" !total;
  if Sys.getenv_opt "FAI_REPORT_RSS" <> None then
    match peak_rss_kib () with Some kib -> Printf.eprintf "fai-peak-rss-kib: %d\n" kib | None -> ()
