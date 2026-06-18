//! Concurrency and networking benchmarks (Fai-only, informational).
//!
//! These measure the runtime's M:N scheduler and I/O reactor through *delivered
//! binaries*: each Fai program is built once with [`fai_driver::build_native`] in
//! untimed setup, then spawned in the timed loop (the same approach as the
//! `algorithms_aot` bench). Each program bakes a large fixed workload so process
//! startup is amortized. There is no cross-language baseline — a green-thread M:N
//! scheduler has no single "fair" peer — so these render as plain timing tables.
//!
//! The suite covers:
//! - `spawn_await` — fan-out/join task throughput (spawn N tasks, sum the awaits);
//! - `channel` — bounded-channel producer/consumer throughput;
//! - `parallel_speedup` — a CPU-bound fan-out run at `FAI_WORKERS=1` vs the host's
//!   default parallelism, whose ratio is the scheduler's parallel speedup;
//! - `tcp_echo` / `udp_echo` — loopback request/response round-trip throughput
//!   over the network reactor.
//!
//! Not run on Windows (the build/link + spawn path mirrors the `algorithms_aot`
//! and daemon e2e benches); it still compiles there so `build --all-targets` keeps
//! it from bitrotting, and the Benchmarks workflow runs on Linux. Run with
//! `cargo bench -p fai-tests --bench concurrency`.

use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use camino::Utf8PathBuf;
use divan::Bencher;
use fai_db::{Db, FaiDatabase};
use fai_driver::build_native;

fn main() {
    // The build/link + spawn path is skipped on Windows (see the module docs); on
    // every other platform this runs the full divan suite.
    #[cfg(not(windows))]
    divan::main();
}

/// A unique temporary path for a built executable.
fn unique_exe(name: &str) -> Utf8PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = Utf8PathBuf::from_path_buf(std::env::temp_dir()).expect("temp dir is UTF-8");
    dir.join(format!(
        "fai-conc-{name}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Links a Fai program into a native executable, returning the path produced.
fn build(name: &str, src: &str) -> Utf8PathBuf {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source(format!("{name}.fai").into(), src.to_owned());
    let file = db.source_file(id).expect("source registered");
    let outcome = build_native(&db, file, &unique_exe(name));
    outcome.artifact.unwrap_or_else(|| panic!("{name} failed to build a native executable"))
}

/// Runs `exe`, optionally pinning the scheduler's worker count, capturing output.
fn run(exe: &Utf8PathBuf, workers: Option<usize>) -> std::process::Output {
    let mut command = Command::new(exe);
    if let Some(n) = workers {
        command.env("FAI_WORKERS", n.to_string());
    }
    command.output().expect("spawn benchmark binary")
}

/// Builds a program, confirms one untimed run exits cleanly (0 also means
/// leak-free) and — when `expect` is given — prints the expected line, then times
/// repeated runs at the given worker count.
fn bench_program(
    bencher: Bencher,
    name: &str,
    src: &str,
    workers: Option<usize>,
    expect: Option<&str>,
) {
    let exe = build(name, src);
    let first = run(&exe, workers);
    assert!(first.status.success(), "{name} exited with {:?}", first.status);
    if let Some(expected) = expect {
        let stdout = String::from_utf8_lossy(&first.stdout);
        assert_eq!(stdout.trim_end(), expected, "{name} produced unexpected output");
    }
    bencher.bench(|| divan::black_box(run(&exe, workers)));
    let _ = std::fs::remove_file(&exe);
}

// ---------------------------------------------------------------------------
// Task throughput: spawn N tasks into a scope and sum their awaited results.
// ---------------------------------------------------------------------------

const SPAWN_AWAIT: &str = r#"module Prog

fan : { concurrency : Concurrency | _ } -> Nursery -> Int / { Concurrency }
let fan env nursery =
  let tasks = List.map (fun i -> env.concurrency.spawn nursery (fun u -> i)) (List.range 0 50000)
  List.foldl (fun acc t -> acc + env.concurrency.await t) 0 tasks

public main : Runtime -> Unit / { Concurrency, Console }
let main runtime =
  runtime.console.writeLine (Int.toString (runtime.concurrency.scope (fan runtime)))
"#;

#[divan::bench]
fn spawn_await(bencher: Bencher) {
    // Sum of 0..49999.
    bench_program(bencher, "SpawnAwait", SPAWN_AWAIT, None, Some("1249975000"));
}

// ---------------------------------------------------------------------------
// Channel throughput: one producer sends N items over a bounded channel; the
// consumer (the root task) drains and sums them.
// ---------------------------------------------------------------------------

const CHANNEL: &str = r#"module Prog

produce : { concurrency : Concurrency | _ } -> Channel Int -> Unit -> Unit / { Concurrency }
let produce env ch u =
  let sent = List.map (fun i -> env.concurrency.send ch i) (List.range 0 100000)
  env.concurrency.close ch

drain : { concurrency : Concurrency | _ } -> Channel Int -> Int -> Int / { Concurrency }
let drain env ch acc =
  match env.concurrency.recv ch with
  | None -> acc
  | Some n -> drain env ch (acc + n)

body : { concurrency : Concurrency | _ } -> Nursery -> Int / { Concurrency }
let body env nursery =
  let ch = env.concurrency.channel 16
  let producer = env.concurrency.spawn nursery (produce env ch)
  drain env ch 0

public main : Runtime -> Unit / { Concurrency, Console }
let main runtime =
  runtime.console.writeLine (Int.toString (runtime.concurrency.scope (body runtime)))
"#;

#[divan::bench]
fn channel(bencher: Bencher) {
    // Sum of 0..99999.
    bench_program(bencher, "Channel", CHANNEL, None, Some("4999950000"));
}

// ---------------------------------------------------------------------------
// Parallel speedup: a CPU-bound fan-out (each task sums a long range with no
// allocation), run with one worker and with the host's default parallelism. The
// ratio of the two medians is the scheduler's speedup.
// ---------------------------------------------------------------------------

const PARALLEL: &str = r#"module Prog

// A pure, allocation-free, CPU-bound loop (self-tail-recursive, so it compiles to
// a constant-stack loop): the sum of 1..n.
work : Int -> Int -> Int
let work n acc = if n <= 0 then acc else work (n - 1) (acc + n)

body : { concurrency : Concurrency | _ } -> Nursery -> Int / { Concurrency }
let body env nursery =
  let tasks = List.map (fun i -> env.concurrency.spawn nursery (fun u -> work 4000000 0)) (List.range 0 32)
  List.foldl (fun acc t -> acc + env.concurrency.await t) 0 tasks

public main : Runtime -> Unit / { Concurrency, Console }
let main runtime =
  runtime.console.writeLine (Int.toString (runtime.concurrency.scope (body runtime)))
"#;

#[divan::bench]
fn parallel_speedup_one_worker(bencher: Bencher) {
    bench_program(bencher, "Parallel", PARALLEL, Some(1), None);
}

#[divan::bench]
fn parallel_speedup_all_workers(bencher: Bencher) {
    // The host's default parallelism (cgroup-/affinity-aware).
    bench_program(bencher, "Parallel", PARALLEL, None, None);
}

// ---------------------------------------------------------------------------
// TCP loopback round-trip throughput: a server task echoes a one-byte message;
// the client sends and reads it back N times over one connection.
// ---------------------------------------------------------------------------

const TCP_ECHO: &str = r#"module Prog

echo : { net : Net | _ } -> Connection -> Unit / { Net }
let echo env conn =
  match env.net.recv conn 64 with
  | Err e -> ()
  | Ok data ->
    if Bytes.length data = 0 then ()
    else
      match env.net.send conn data with
      | Err e -> ()
      | Ok u -> echo env conn

serve : { net : Net | _ } -> Listener -> Unit / { Net }
let serve env listener =
  match env.net.accept listener with
  | Err e -> ()
  | Ok conn -> echo env conn

pingpong : { net : Net | _ } -> Connection -> Int -> Int / { Net }
let pingpong env conn n =
  if n <= 0 then 0
  else
    match env.net.send conn (Bytes.fromString "x") with
    | Err e -> 0 - 1
    | Ok u ->
      match env.net.recv conn 64 with
      | Err e -> 0 - 2
      | Ok r -> pingpong env conn (n - 1)

session : { concurrency : Concurrency, net : Net | _ } -> Listener -> Int -> Nursery -> Int / { Concurrency, Net }
let session env listener port nursery =
  let server = env.concurrency.spawn nursery (fun u -> serve env listener)
  match env.net.connect "127.0.0.1" port with
  | Err e -> 0 - 3
  | Ok conn ->
    let result = pingpong env conn 5000
    let closed = env.net.close conn
    result

public main : Runtime -> Unit / { Concurrency, Console, Net }
let main runtime =
  match runtime.net.listen 0 with
  | Err e -> runtime.console.writeLine ("listen: " ++ e)
  | Ok listener ->
    let port = runtime.net.localPort listener
    runtime.console.writeLine (Int.toString (runtime.concurrency.scope (session runtime listener port)))
"#;

#[divan::bench]
fn tcp_echo(bencher: Bencher) {
    bench_program(bencher, "TcpEcho", TCP_ECHO, None, Some("0"));
}

// ---------------------------------------------------------------------------
// UDP loopback round-trip throughput: a server task echoes each datagram back to
// its sender; the client sends and reads it back N times.
// ---------------------------------------------------------------------------

const UDP_ECHO: &str = r#"module Prog

serveUdp : { net : Net | _ } -> UdpSocket -> Int -> Unit / { Net }
let serveUdp env socket n =
  if n <= 0 then ()
  else
    match env.net.udpRecv socket 64 with
    | Err e -> ()
    | Ok (data, host, port) ->
      match env.net.udpSend socket host port data with
      | Err e -> ()
      | Ok u -> serveUdp env socket (n - 1)

ping : { net : Net | _ } -> UdpSocket -> Int -> Int -> Int / { Net }
let ping env socket serverPort n =
  if n <= 0 then 0
  else
    match env.net.udpSend socket "127.0.0.1" serverPort (Bytes.fromString "x") with
    | Err e -> 0 - 1
    | Ok u ->
      match env.net.udpRecv socket 64 with
      | Err e -> 0 - 2
      | Ok (r, h, p) -> ping env socket serverPort (n - 1)

session : { concurrency : Concurrency, net : Net | _ } -> UdpSocket -> UdpSocket -> Int -> Nursery -> Int / { Concurrency, Net }
let session env server client serverPort nursery =
  let s = env.concurrency.spawn nursery (fun u -> serveUdp env server 5000)
  ping env client serverPort 5000

public main : Runtime -> Unit / { Concurrency, Console, Net }
let main runtime =
  match runtime.net.udpBind 0 with
  | Err e -> runtime.console.writeLine ("bind server: " ++ e)
  | Ok server ->
    let serverPort = runtime.net.udpLocalPort server
    match runtime.net.udpBind 0 with
    | Err e -> runtime.console.writeLine ("bind client: " ++ e)
    | Ok client ->
      runtime.console.writeLine (Int.toString (runtime.concurrency.scope (session runtime server client serverPort)))
"#;

#[divan::bench]
fn udp_echo(bencher: Bencher) {
    bench_program(bencher, "UdpEcho", UDP_ECHO, None, Some("0"));
}
