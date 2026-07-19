//! End-to-end integration test — the toolkit used exactly as a third party would.
//!
//! This file compiles as a *separate crate* against only `sovds`'s **public** API,
//! so it proves the public surface is complete and ergonomic enough to build the
//! whole pipeline from the outside (an independence guarantee the in-crate unit
//! tests, which can reach private paths, cannot give). The domain here — a message
//! queue's depth — is deliberately unrelated to the router.

use sovds::dataset;
use sovds::domain::{Accept, Domain, ParseError};
use sovds::episode::{segment, EpisodeEnd, SegConfig};
use sovds::injection;
use sovds::readiness;
use sovds::record::{Record, Source, Tick};
use sovds::recorder::Recorder;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Queue {
    Drained,
    Backlogged,
    Stalled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QueueDepth(u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Broker;

#[derive(Default)]
struct Builder {
    depth: Option<u32>,
    scaled: bool,
}

impl Domain for Broker {
    type Obs = QueueDepth;
    type Fault = Queue;
    type Annot = bool; // did we scale workers?
    type Builder = Builder;

    fn healthy() -> Queue {
        Queue::Drained
    }
    fn faults() -> &'static [Queue] {
        &[Queue::Drained, Queue::Backlogged, Queue::Stalled]
    }
    fn fault_label(f: Queue) -> &'static str {
        match f {
            Queue::Drained => "drained",
            Queue::Backlogged => "backlogged",
            Queue::Stalled => "stalled",
        }
    }
    fn parse_fault(s: &str) -> Option<Queue> {
        match s {
            "drained" => Some(Queue::Drained),
            "backlogged" => Some(Queue::Backlogged),
            "stalled" => Some(Queue::Stalled),
            _ => None,
        }
    }
    fn write_obs<W: fmt::Write>(w: &mut W, o: &QueueDepth) -> fmt::Result {
        write!(w, " depth={}", o.0)
    }
    fn write_annot<W: fmt::Write>(w: &mut W, scaled: &bool) -> fmt::Result {
        write!(w, " scaled={}", u8::from(*scaled))
    }
    fn accept(b: &mut Builder, key: &str, value: &str) -> Result<Accept, ParseError> {
        match key {
            "depth" => {
                b.depth = Some(value.parse().map_err(|_| ParseError::BadField)?);
                Ok(Accept::Consumed)
            }
            "scaled" => {
                b.scaled = value == "1";
                Ok(Accept::Consumed)
            }
            _ => Ok(Accept::Unknown),
        }
    }
    fn finish(b: Builder) -> Result<(QueueDepth, bool), ParseError> {
        Ok((
            QueueDepth(b.depth.ok_or(ParseError::MissingField)?),
            b.scaled,
        ))
    }
}

/// A tiny rule set: depth over 1000 stalls, over 100 backlogs, else drained.
fn classify(d: QueueDepth) -> Queue {
    if d.0 > 1000 {
        Queue::Stalled
    } else if d.0 > 100 {
        Queue::Backlogged
    } else {
        Queue::Drained
    }
}

fn record(tick: u64, depth: u32, inject: Option<Queue>) -> Record<Broker> {
    let obs = QueueDepth(depth);
    let class = classify(obs);
    Record {
        tick: Tick(tick),
        wall_time: 1_700_000_000u64.saturating_add(tick.saturating_mul(60)),
        probed: true,
        obs,
        class,
        annot: !matches!(class, Queue::Drained),
        source: Source::Live,
        label: inject,
    }
}

fn temp_path() -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("sovds-it-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

#[test]
fn a_third_party_domain_drives_the_whole_pipeline_through_the_public_api() {
    // A simulated run: healthy, an INJECTED backlog window, recovery, then an
    // ORGANIC stall, then healthy.
    let bl = Queue::Backlogged;
    let mut records: Vec<Record<Broker>> = Vec::new();
    for t in 0..5 {
        records.push(record(t, 10, None)); // drained
    }
    for t in 5..9 {
        records.push(record(t, 250, Some(bl))); // induced backlog
    }
    for t in 9..15 {
        records.push(record(t, 12, None)); // recovery
    }
    for t in 15..18 {
        records.push(record(t, 5000, None)); // organic stall
    }
    for t in 18..21 {
        records.push(record(t, 8, None)); // healthy again
    }

    // Persist through the recorder and load back from disk — the format must
    // round-trip through the public serializer/parser.
    let path = temp_path();
    let recorder = Recorder::new(path.clone(), 5_000_000, 3);
    for r in &records {
        assert!(recorder.append(r).is_ok(), "append must succeed");
    }
    let corpus = dataset::load::<Broker>(&path, 3);
    let _ = std::fs::remove_file(&path);
    assert_eq!(corpus.records.len(), 21, "all records loaded");
    assert_eq!(corpus.skipped, 0, "nothing unparseable");

    // Episodes: one backlog, one stall.
    let episodes = segment::<Broker>(&corpus.records, SegConfig::new());
    assert_eq!(episodes.len(), 2, "two fault episodes");
    let backlog = episodes.iter().find(|e| e.class == Queue::Backlogged);
    assert!(
        matches!(backlog, Some(e) if e.injected == Some(bl) && e.end == EpisodeEnd::Recovered),
        "the injected backlog episode is labelled and recovered"
    );
    let stall = episodes.iter().find(|e| e.class == Queue::Stalled);
    assert!(
        matches!(stall, Some(e) if e.injected.is_none()),
        "the stall episode is organic (unlabelled)"
    );

    // Injection detection: the backlog window was diagnosed correctly.
    let detection: Vec<_> = injection::accuracy(&corpus.records).collect();
    let d = detection.iter().find(|d| d.class == Queue::Backlogged);
    assert!(
        matches!(d, Some(s) if s.windows == 1 && s.detected == 1 && s.missed == 0),
        "the induced backlog was detected in its one window"
    );

    // Rollback safety: the induced fault recovered.
    let safety = injection::safety(&corpus.records);
    assert_eq!(safety.windows, 1);
    assert_eq!(safety.recovered, 1);
    assert_eq!(safety.stuck, 0);

    // Readiness: one episode of each fault class.
    let readiness = readiness::summarize(&episodes);
    let backlog_ready = readiness.iter().find(|c| c.class == Queue::Backlogged);
    assert!(matches!(backlog_ready, Some(c) if c.count == 1 && c.recovered == 1));
}
