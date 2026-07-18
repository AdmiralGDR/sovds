//! Integrating `sovds` into a third-party project — a worked example.
//!
//! This is a **host resource monitor**: a completely different domain from the
//! router the toolkit was extracted from. It shows the whole contract — implement
//! [`Domain`] once for your world, then get the entire dataset pipeline (durable
//! recording, tolerant loading, episode segmentation, injection detection +
//! rollback safety, training-readiness) for free, with zero third-party deps.
//!
//! Run it:
//! ```text
//! cargo run -p sovds --example host_monitor
//! ```

use sovds::dataset;
use sovds::domain::{Accept, Domain, ParseError};
use sovds::episode::{Episode, SegConfig, Segmenter};
use sovds::record::{Record, Source, Tick};
use sovds::recorder::Recorder;
use sovds::{injection, readiness};
use std::fmt;

// --- 1. The fault vocabulary: what can go wrong on this host. -----------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Res {
    Ok,
    CpuSaturated,
    MemPressure,
}

// --- 2. The observation: one sample of the host's telemetry. ------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Load {
    cpu_pct: u8,
    mem_pct: u8,
}

// --- 3. The domain tag + a token accumulator for parsing. ---------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Host;

#[derive(Default)]
struct Builder {
    cpu: Option<u8>,
    mem: Option<u8>,
    shed: bool,
}

impl Domain for Host {
    type Obs = Load;
    type Fault = Res;
    /// The annotation carried per record: did we shed load in response?
    type Annot = bool;
    type Builder = Builder;

    fn healthy() -> Res {
        Res::Ok
    }
    fn faults() -> &'static [Res] {
        &[Res::Ok, Res::CpuSaturated, Res::MemPressure]
    }
    fn fault_label(f: Res) -> &'static str {
        match f {
            Res::Ok => "ok",
            Res::CpuSaturated => "cpu_saturated",
            Res::MemPressure => "mem_pressure",
        }
    }
    fn parse_fault(s: &str) -> Option<Res> {
        match s {
            "ok" => Some(Res::Ok),
            "cpu_saturated" => Some(Res::CpuSaturated),
            "mem_pressure" => Some(Res::MemPressure),
            _ => None,
        }
    }
    fn write_obs<W: fmt::Write>(w: &mut W, o: &Load) -> fmt::Result {
        write!(w, " cpu={} mem={}", o.cpu_pct, o.mem_pct)
    }
    fn write_annot<W: fmt::Write>(w: &mut W, shed: &bool) -> fmt::Result {
        write!(w, " shed={}", u8::from(*shed))
    }
    fn accept(b: &mut Builder, key: &str, value: &str) -> Result<Accept, ParseError> {
        match key {
            "cpu" => {
                b.cpu = Some(value.parse().map_err(|_| ParseError::BadField)?);
                Ok(Accept::Consumed)
            }
            "mem" => {
                b.mem = Some(value.parse().map_err(|_| ParseError::BadField)?);
                Ok(Accept::Consumed)
            }
            "shed" => {
                b.shed = value == "1";
                Ok(Accept::Consumed)
            }
            _ => Ok(Accept::Unknown),
        }
    }
    fn finish(b: Builder) -> Result<(Load, bool), ParseError> {
        Ok((
            Load {
                cpu_pct: b.cpu.ok_or(ParseError::MissingField)?,
                mem_pct: b.mem.ok_or(ParseError::MissingField)?,
            },
            b.shed,
        ))
    }
}

/// A trivial rule set: the "brain" for this domain. CPU dominates memory.
fn classify(l: Load) -> Res {
    if l.cpu_pct > 90 {
        Res::CpuSaturated
    } else if l.mem_pct > 90 {
        Res::MemPressure
    } else {
        Res::Ok
    }
}

/// Build one record: classify the load, and carry an optional ground-truth
/// injection label (set when we induced the fault on purpose).
fn record(tick: u64, cpu: u8, mem: u8, inject: Option<Res>) -> Record<Host> {
    let obs = Load {
        cpu_pct: cpu,
        mem_pct: mem,
    };
    let class = classify(obs);
    Record {
        tick: Tick(tick),
        wall_time: 1_700_000_000u64.saturating_add(tick.saturating_mul(60)),
        probed: true,
        obs,
        class,
        annot: !matches!(class, Res::Ok), // shed when not healthy
        source: Source::Live,
        label: inject,
    }
}

fn main() {
    // A simulated collection run: healthy baseline, a DELIBERATELY INJECTED
    // cpu-saturation window (label = cpu_saturated), recovery, then an ORGANIC
    // memory-pressure episode (no label), then healthy again.
    let mut records: Vec<Record<Host>> = Vec::new();
    let cpu = Res::CpuSaturated;
    for t in 0..5 {
        records.push(record(t, 25, 40, None)); // healthy baseline
    }
    for t in 5..9 {
        records.push(record(t, 96, 45, Some(cpu))); // induced CPU saturation
    }
    for t in 9..15 {
        records.push(record(t, 30, 42, None)); // recovery
    }
    for t in 15..18 {
        records.push(record(t, 35, 93, None)); // organic memory pressure
    }
    for t in 18..21 {
        records.push(record(t, 28, 40, None)); // healthy again
    }

    // Persist through the durable recorder, then load back — proving the format
    // round-trips on disk exactly as any real collector would.
    let mut path = std::env::temp_dir();
    path.push(format!("sovds-host-example-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let recorder = Recorder::new(path.clone(), 5_000_000, 3);
    for r in &records {
        if recorder.append(r).is_err() {
            eprintln!("host_monitor: could not write record");
        }
    }
    let corpus = dataset::load::<Host>(&path, 3);
    let _ = std::fs::remove_file(&path);
    println!(
        "Loaded {} record(s) from disk, {} skipped.\n",
        corpus.records.len(),
        corpus.skipped
    );

    // Segment into fault episodes.
    let mut seg = Segmenter::new(SegConfig::new());
    let mut episodes: Vec<Episode<Host>> = Vec::new();
    for r in &corpus.records {
        if let Some(ep) = seg.observe(r) {
            episodes.push(ep);
        }
    }
    if let Some(ep) = seg.finish() {
        episodes.push(ep);
    }

    println!("Fault episodes:");
    for ep in &episodes {
        let tag = match ep.injected {
            Some(c) => Host::fault_label(c),
            None => "organic",
        };
        println!(
            "  {:<13} {:>3} cycles, {:>4}s, {:?}  [{tag}]",
            Host::fault_label(ep.class),
            ep.records,
            ep.duration_secs(),
            ep.end,
        );
    }

    println!("\nTraining readiness (independent episodes per class):");
    for c in readiness::summarize(&episodes) {
        println!(
            "  {:<13} {} episode(s), {} recovered",
            Host::fault_label(c.class),
            c.count,
            c.recovered
        );
    }

    println!("\nInjection detection (did the rules catch what we induced?):");
    for d in injection::accuracy(&corpus.records) {
        let manifested = d.detected.saturating_add(d.missed);
        let rate = d
            .detected
            .saturating_mul(100)
            .checked_div(manifested)
            .map_or_else(|| "n/a".to_string(), |r| format!("{r}%"));
        println!(
            "  {:<13} {}/{} windows detected ({rate}), {} silent",
            Host::fault_label(d.class),
            d.detected,
            d.windows,
            d.silent
        );
    }

    let safety = injection::safety(&corpus.records);
    println!(
        "\nInjection rollback safety: {}/{} windows recovered.",
        safety.recovered, safety.windows
    );
}
