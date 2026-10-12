# Performance report presentation

This package turns versioned, validated `report-data.json` into the three tracker
bodies. It owns names, wording, units, overview selection and Markdown layout.
Rust owns evidence validation, ratios, omission reasons and statistics.

```sh
# Fast presentation preview: Python only.
python scripts/performance_report path/to/report-data.json /tmp/report-preview
python -m unittest discover -s scripts/performance_report/tests -v

# Validate a retained measurement and render with the current presentation.
python scripts/performance.py render path/to/snapshot
python scripts/performance.py publish path/to/snapshot
```

There are two end-to-end renderer fixtures: a complete report and omitted/missing
comparisons. They cover all three documents, overview/full-detail behavior, units
and supplied statistics without a compiler build. Presentation-only changes use
the Python CI lane and do not change the compiler bundle fingerprint.

`report-data.json` schema 1 contains `measurement` metadata, `runtime` cases and
summaries, `memory` cases/summaries/input definitions, and `allocations` guards and
control pairs. Numeric values retain full precision; unavailable ratios are null
with explicit omission reasons. The renderer displays these values rather than
recomputing their scientific meaning. A schema change must be deliberate on both
sides of this boundary.

The Rust exporter is invoked as `performance-report export SNAPSHOT OUTPUT`.
For a snapshot whose original executable predates that command, select a newer
exporter once:

```sh
python scripts/performance.py render path/to/snapshot \
  --validator /path/to/performance-report --validator-revision FULL_COMMIT_SHA
```

The selected executable is copied into the snapshot's presentation directory and
fingerprinted. The original measurement manifest and executable remain available.
`render-info.json` records the snapshot/data hashes, validator identity, Python
version and exact presentation sources. Publication revalidates the evidence and
archives a new presentation revision when that identity changes.
