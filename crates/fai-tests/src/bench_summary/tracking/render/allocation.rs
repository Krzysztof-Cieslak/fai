//! Within-Fai memory guarantees and source-identical optimization controls.

use super::display::{bytes, number};
use super::*;

fn counter(row: &AllocationSample, field: &str) -> u64 {
    row.counters[field].as_u64().expect("validated allocation counter")
}

fn additional_live(row: &AllocationSample) -> u64 {
    counter(row, "peakLiveBytes") - counter(row, "baselineLiveBytes")
}

fn retained(row: &AllocationSample) -> u64 {
    counter(row, "retainedSlabBytes")
        + counter(row, "retainedMappedBytes")
        + counter(row, "retainedSystemBytes")
}

fn name(id: &str) -> &str {
    match id {
        "Fusion" => "Sequence fusion",
        "Scalars" => "Scalar replacement",
        "SharedList" => "Shared list",
        "UniqueList" => "Unique list",
        _ => id,
    }
}

pub(super) fn report(snapshot: &Snapshot, evidence: &str) -> String {
    let mut text =
        header(snapshot, "perceus", "Memory guarantees and allocation evidence", evidence);
    text.push_str("Perceus is Fai's reference-counting and storage-reuse model. These checks verify ownership, reuse and memory release within Fai.\n\n## Memory guarantees\n\n**All required memory checks passed.**\n\n| Check suite | Tests passed | What it checks |\n|---|---:|---|\n");
    for (id, label, scope) in [
        (
            "perceus_space",
            "Live memory and ownership",
            "Reference balance, reuse, sharing and resource lifetimes",
        ),
        (
            "perceus_storage",
            "Retained runtime storage",
            "Bounded allocator storage and release of large buffers",
        ),
    ] {
        let step =
            snapshot.manifest.steps.iter().find(|step| step.name == id).expect("validated guard");
        let _ = writeln!(text, "| {label} | {} | {scope} |", number(step.passed.unwrap_or(0)));
    }
    let mut groups = BTreeMap::<_, Vec<_>>::new();
    for row in &snapshot.reports["allocations"].allocations {
        groups.entry((row.algorithm.as_str(), row.size)).or_default().push(row);
    }
    let pairs: Vec<_> = groups
        .into_iter()
        .map(|((id, size), rows)| {
            let disabled =
                *rows.iter().find(|row| row.control != "enabled").expect("validated control");
            let enabled =
                *rows.iter().find(|row| row.control == "enabled").expect("validated control");
            (id, size, disabled, enabled)
        })
        .collect();
    let mut largest = BTreeMap::<_, i64>::new();
    for (id, size, _, _) in &pairs {
        largest.entry(*id).and_modify(|old| *old = (*old).max(*size)).or_insert(*size);
    }
    text.push_str("\n## Effect of memory optimizations\n\nArrows mean **optimization disabled → enabled** on the same source and input. Reference counting stays active in both runs. This overview uses each benchmark's largest input.\n\n| Benchmark | Input size | Allocation requests | Extra peak live memory | Retained runtime storage |\n|---|---:|---:|---:|---:|\n");
    for &(id, size, disabled, enabled) in
        pairs.iter().filter(|(id, size, _, _)| largest[id] == *size)
    {
        let _ = writeln!(
            text,
            "| {} | {} | {} → {} | {} → {} | {} → {} |",
            name(id),
            number(size),
            number(counter(disabled, "allocations")),
            number(counter(enabled, "allocations")),
            bytes(additional_live(disabled)),
            bytes(additional_live(enabled)),
            bytes(retained(disabled)),
            bytes(retained(enabled))
        );
    }
    text.push_str("\nExtra live memory is above the retained harness baseline. Runtime storage includes allocator-owned memory even after its objects are released. Every measured run finishes with its baseline live-object count and live bytes.\n\n");
    details(&mut text, "All inputs and allocation counters");
    text.push_str("| Benchmark | Input size | Disabled control | Allocation requests | Allocated memory | Extra peak live memory | Peak runtime storage | Retained runtime storage |\n|---|---:|---|---:|---:|---:|---:|---:|\n");
    for (id, size, disabled, enabled) in pairs {
        let _ = writeln!(
            text,
            "| {} | {} | {} | {} → {} | {} → {} | {} → {} | {} → {} | {} → {} |",
            name(id),
            number(size),
            escape(&disabled.control),
            number(counter(disabled, "allocations")),
            number(counter(enabled, "allocations")),
            bytes(counter(disabled, "allocatedBytes")),
            bytes(counter(enabled, "allocatedBytes")),
            bytes(additional_live(disabled)),
            bytes(additional_live(enabled)),
            bytes(counter(disabled, "peakBackingBytes")),
            bytes(counter(enabled, "peakBackingBytes")),
            bytes(retained(disabled)),
            bytes(retained(enabled))
        );
    }
    end_details(&mut text);
    finish(
        snapshot,
        "perceus",
        evidence,
        "Each pair uses identical Fai source, compiler and input; only the named optimization changes. The measurements use an optimized build with allocation counters.\n\nThe accounting domain is `fai-object-storage-v1`. Allocation requests include headers and pooled allocations; reset/reuse avoids a request. Live bytes exclude dead recycling cells. Runtime storage includes owned slabs, mappings and system allocations. Host-language temporaries, allocator arenas and OS page rounding are outside these counters. These figures are distinct from whole-process RSS and cross-language memory comparisons.",
        &mut text,
    );
    text
}
