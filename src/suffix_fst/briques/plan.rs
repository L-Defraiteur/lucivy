//! The FST phase of a query on a shard dictionary, done once per shard,
//! ahead of the segments.
//!
//! On a dictionary index (`sfx_version` 4) every segment of a shard walks
//! the same FST for the same query. The walks are memoized on the shared
//! reader (`FstMemo`), but a memo only decides *who computes*: the first
//! segment to ask, on one thread, while the others wait — the CPU total of
//! the v3 per-segment walk, spread over one thread instead of the pool
//! (`sched term` cold 3.2 → 70 ms on 30 000 files, September 2026).
//!
//! The plan fills those cells before the segments start, as a fan-out of
//! scheduler tasks with nobody waiting under them: the cells of one wave
//! are independent (a cell's computation never touches the memo), and the
//! next wave — the remainders of the chains — is derived from what the
//! previous one stored. The segments then find every cell done; a cell the
//! plan did not anticipate is computed inline as before, so the plan is an
//! optimisation, never a condition of correctness.
//!
//! What a literal needs (see `composite::find_literal_v3`):
//! - its candidates per partition (`fst_candidates_v3`);
//! - its chunk splits (`falling_walk_chunks`) and, for every remainder the
//!   chain builder may reach — every suffix of the lowercased query — the
//!   anchored candidate *count* of the remainder (the chain carries the
//!   remainder as a prefix alternative, tested on the text — `Alts::Prefix`)
//!   and its own splits;
//! - the same on the word partition when separators are relaxed;
//! - strict: the remainders `query[h..]` for `h <= len / 2`, roots of the
//!   occurrences anchored on their second token, whose anchored candidates
//!   are listed (they are a chain's first position).
//!
//! All of it in one wave: the remainders are known without walking.
//!
//! A fuzzy query first needs the candidate counts of its n-grams and of
//! every piece it may cut (`fuzzy_generator`), then the literals of the
//! pieces it chose, or the candidates of the rarest n-grams. A regex needs
//! its required literals.

use std::collections::HashSet;

use crate::schema::Field;
use crate::suffix_fst::builder::SI0_PREFIX;
use crate::suffix_fst::builder_v3::SI_STRIPPED_PREFIX;
use crate::suffix_fst::dictionary::DictionaryField;
use crate::suffix_fst::file_v3::SfxFileReaderV3;
use crate::SegmentReader;

use super::composite::{self, FuzzyGenerator};
use super::fst_walk::{
    self, MEMO_TAG_CANDIDATES, MEMO_TAG_COUNT, MEMO_TAG_WALK_CHUNKS, MEMO_TAG_WALK_WORDS,
};

/// `V3_PLAN=0` skips the plan (the segments compute the cells themselves,
/// as before): the A/B of the plan, and the measure of its own cost.
fn plan_enabled() -> bool {
    std::env::var("V3_PLAN").map_or(true, |v| v != "0")
}

// ─── Cells ────────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq, Eq, Hash)]
enum CellKind {
    Candidates(u8),
    Count(u8),
    WalkChunks,
    WalkWords,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Cell {
    dict: usize,
    key: String,
    kind: CellKind,
}

impl Cell {
    fn tag_and_flags(&self) -> (u8, u8) {
        match self.kind {
            CellKind::Candidates(p) => (MEMO_TAG_CANDIDATES, fst_walk::partition_flags(p)),
            CellKind::Count(p) => (MEMO_TAG_COUNT, fst_walk::partition_flags(p)),
            CellKind::WalkChunks => (MEMO_TAG_WALK_CHUNKS, 0),
            CellKind::WalkWords => (MEMO_TAG_WALK_WORDS, 0),
        }
    }

    fn compute(&self, reader: &SfxFileReaderV3) {
        let memo = reader.memo().expect("plan cell on a reader without memo");
        match self.kind {
            CellKind::Candidates(p) => {
                let _ = fst_walk::memo_candidates_in_partition(reader, memo, &self.key, p);
            }
            CellKind::Count(p) => {
                let _ = fst_walk::memo_count_in_partition(reader, memo, &self.key, p);
            }
            CellKind::WalkChunks => {
                let _ = fst_walk::memo_walk_chunks(reader, memo, &self.key);
            }
            CellKind::WalkWords => {
                let _ = fst_walk::memo_walk_words(reader, memo, &self.key);
            }
        }
    }
}

// ─── Jobs ─────────────────────────────────────────────────────────────────

const DONE: usize = usize::MAX;

/// The FST work of one literal on one dictionary: one wave.
///
/// The remainders a chain builder reaches are suffixes of the lowercased
/// query, so every char-boundary suffix is planned at once — a superset
/// of what the segments will ask, in one wave instead of one per chain
/// depth (four or five, at 0.3 ms of scheduling latency each, for cells
/// worth 0.1 ms of CPU). The cells of the suffixes never reached cost a
/// walk and a count each, spread over the pool.
struct LiteralJob {
    dict: usize,
    /// The literal as `find_literal_v3` receives it (original case).
    query: String,
    /// Lowercased: what the remainders are cut from.
    lower: String,
    anchor_start: bool,
    strict: bool,
    /// Whether the chunk chains are walked at all (relaxed mode skips them
    /// when no word of the dictionary reaches the word suffix cap).
    chunk: bool,
    /// Whether the word pipeline runs (relaxed mode).
    word: bool,
    /// Strict: heads up to this length are anchored on the second token.
    half: usize,
    done: bool,
}

impl LiteralJob {
    fn new(dict: usize, query: &str, anchor_start: bool, strict: bool, may_have_long_words: bool) -> Self {
        let chunk = strict
            || may_have_long_words
            || std::env::var("V3_RELAXED_CHUNK_CHAINS").is_ok_and(|v| v == "1");
        let half = if strict { query.len() / 2 } else { 0 };
        Self {
            dict,
            query: query.to_string(),
            lower: query.to_lowercase(),
            anchor_start,
            strict,
            chunk,
            word: !strict,
            half,
            done: false,
        }
    }

    fn cell(&self, key: &str, kind: CellKind) -> Cell {
        Cell { dict: self.dict, key: key.to_string(), kind }
    }

    /// The cells of the job; empty once given.
    fn wave(&mut self) -> Vec<Cell> {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        let mut cells = Vec::new();
        // The root: its candidates per partition, its walks.
        for &p in fst_walk::candidate_partitions(self.anchor_start, self.strict) {
            cells.push(self.cell(&self.query, CellKind::Candidates(p)));
        }
        if self.chunk {
            cells.push(self.cell(&self.query, CellKind::WalkChunks));
        }
        if self.word {
            cells.push(self.cell(&self.query, CellKind::WalkWords));
        }
        // Every suffix: what `build_chains_from_splits` asks for a
        // remainder — its anchored count (not for a short one, see
        // `PREFIX_ASSUMED_MAX_BYTES`) and its walk — and, for a second-token
        // root (`h <= half`, strict), the SI0 candidate list.
        for at in 1..self.lower.len() {
            if !self.lower.is_char_boundary(at) {
                continue;
            }
            let rem = &self.lower[at..];
            let count = rem.len() > fst_walk::PREFIX_ASSUMED_MAX_BYTES;
            if self.chunk {
                if count {
                    cells.push(self.cell(rem, CellKind::Count(SI0_PREFIX)));
                }
                cells.push(self.cell(rem, CellKind::WalkChunks));
                if at <= self.half {
                    cells.push(self.cell(rem, CellKind::Candidates(SI0_PREFIX)));
                }
            }
            if self.word {
                if count {
                    cells.push(self.cell(rem, CellKind::Count(SI0_PREFIX)));
                    cells.push(self.cell(rem, CellKind::Count(SI_STRIPPED_PREFIX)));
                }
                cells.push(self.cell(rem, CellKind::WalkWords));
            }
        }
        cells
    }
}

/// The FST work of one fuzzy query on one dictionary: the counts, then the
/// generator's own cells (literal jobs for the pieces, candidates for the
/// kept n-grams).
struct FuzzyJob {
    dict: usize,
    query: String,
    distance: u8,
    strict: bool,
    may_have_long_words: bool,
    stage: usize,
}

impl FuzzyJob {
    /// Returns the cells of the next wave and the literal jobs it spawns.
    fn wave(&mut self, readers: &[SfxFileReaderV3]) -> (Vec<Cell>, Vec<LiteralJob>) {
        let mut cells = Vec::new();
        let mut spawned = Vec::new();
        match self.stage {
            0 => {
                let lower = self.query.to_lowercase();
                let (ngrams, _, _) = composite::generate_trigrams(&self.query, self.distance);
                let mut wanted: Vec<String> = ngrams;
                // Every piece `choose_pieces` may price: [a, b) on char
                // boundaries, at least PIECE_MIN_BYTES long.
                let cuts: Vec<usize> = std::iter::once(0)
                    .chain((1..lower.len()).filter(|&i| lower.is_char_boundary(i)))
                    .chain(std::iter::once(lower.len()))
                    .collect();
                for (i, &a) in cuts.iter().enumerate() {
                    for &b in &cuts[i + 1..] {
                        if b - a >= composite::PIECE_MIN_BYTES {
                            wanted.push(lower[a..b].to_string());
                        }
                    }
                }
                wanted.sort();
                wanted.dedup();
                for q in wanted {
                    for &p in fst_walk::candidate_partitions(false, self.strict) {
                        cells.push(Cell { dict: self.dict, key: q.clone(), kind: CellKind::Count(p) });
                    }
                }
                self.stage = 1;
            }
            1 => {
                let reader = &readers[self.dict];
                let (ngrams, _, _, generator, _) =
                    composite::fuzzy_generator(reader, &self.query, self.distance, self.strict);
                let lower = self.query.to_lowercase();
                match generator {
                    FuzzyGenerator::Pieces(pieces) => {
                        for (a, b) in pieces {
                            spawned.push(LiteralJob::new(self.dict, &lower[a..b], false, self.strict, self.may_have_long_words));
                        }
                    }
                    FuzzyGenerator::Pivot(keep) => {
                        // The rarest `keep` by count, as `resolve_all_trigrams`
                        // ranks them (stable sort).
                        let mut selectivity: Vec<(usize, usize)> = ngrams.iter().enumerate()
                            .map(|(i, g)| (i, fst_walk::fst_candidates_count_v3(reader, g, false, self.strict)))
                            .collect();
                        selectivity.sort_by_key(|&(_, c)| c);
                        selectivity.truncate(keep.max(1));
                        for (i, _) in selectivity {
                            for &p in fst_walk::candidate_partitions(false, self.strict) {
                                cells.push(Cell { dict: self.dict, key: ngrams[i].clone(), kind: CellKind::Candidates(p) });
                            }
                        }
                    }
                    FuzzyGenerator::AllNgrams => {
                        for g in &ngrams {
                            for &p in fst_walk::candidate_partitions(false, self.strict) {
                                cells.push(Cell { dict: self.dict, key: g.clone(), kind: CellKind::Candidates(p) });
                            }
                        }
                    }
                }
                self.stage = DONE;
            }
            _ => {}
        }
        (cells, spawned)
    }
}

// ─── Planner ──────────────────────────────────────────────────────────────

/// What a plan did: for the `[plan]` profile line.
#[derive(Default, Debug)]
pub struct PlanReport {
    pub waves: usize,
    pub cells_computed: usize,
    pub cells_held: usize,
    pub wall: std::time::Duration,
    /// Under `V3_PROFILE`: per wave, (cells computed, wall, CPU sum, the
    /// slowest cell as "kind key" and its time).
    pub wave_profile: Vec<(usize, std::time::Duration, std::time::Duration, String, std::time::Duration)>,
}

impl std::fmt::Display for CellKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CellKind::Candidates(p) => write!(f, "cand/{p:02x}"),
            CellKind::Count(p) => write!(f, "count/{p:02x}"),
            CellKind::WalkChunks => write!(f, "walk-chunks"),
            CellKind::WalkWords => write!(f, "walk-words"),
        }
    }
}

struct Planner {
    readers: Vec<SfxFileReaderV3>,
    literals: Vec<LiteralJob>,
    fuzzies: Vec<FuzzyJob>,
    report: PlanReport,
}

impl Planner {
    fn run(mut self) -> PlanReport {
        let t0 = std::time::Instant::now();
        loop {
            let mut cells: Vec<Cell> = Vec::new();
            for job in &mut self.literals {
                cells.extend(job.wave());
            }
            let mut spawned = Vec::new();
            for job in &mut self.fuzzies {
                let (c, s) = job.wave(&self.readers);
                cells.extend(c);
                spawned.extend(s);
            }
            let any_new_job = !spawned.is_empty();
            self.literals.extend(spawned);
            if cells.is_empty() && !any_new_job {
                break;
            }
            self.report.waves += 1;
            self.fill(cells);
        }
        self.report.wall = t0.elapsed();
        self.report
    }

    /// Compute the cells not held yet, one scheduler task each, and wait
    /// for all of them. The tasks never wait on anything themselves.
    fn fill(&mut self, cells: Vec<Cell>) {
        let mut seen: HashSet<Cell> = HashSet::new();
        let mut todo: Vec<Cell> = Vec::new();
        for c in cells {
            if !seen.insert(c.clone()) {
                continue;
            }
            let (tag, flags) = c.tag_and_flags();
            let memo = self.readers[c.dict].memo().expect("plan on a reader without memo");
            if memo.contains(tag, c.key.as_bytes(), flags) {
                self.report.cells_held += 1;
            } else {
                todo.push(c);
            }
        }
        if todo.is_empty() {
            return;
        }
        self.report.cells_computed += todo.len();
        let profiling = super::profile::enabled();
        let t_wave = std::time::Instant::now();
        let n = todo.len();
        // Under the profile: (time, "kind key") of every cell of the wave.
        let timings: std::sync::Arc<std::sync::Mutex<Vec<(std::time::Duration, String)>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        if todo.len() <= 2 {
            // Not worth the scheduling latency.
            for cell in &todo {
                let t = std::time::Instant::now();
                cell.compute(&self.readers[cell.dict]);
                if profiling {
                    timings.lock().unwrap().push((t.elapsed(), format!("{} {:?}", cell.kind, cell.key)));
                }
            }
        } else {
            let scheduler = crate::actor::scheduler::global_scheduler();
            let mut rxs = Vec::with_capacity(todo.len());
            for cell in todo {
                let reader = self.readers[cell.dict].clone();
                let timings = profiling.then(|| timings.clone());
                rxs.push(scheduler.submit_task(crate::actor::Priority::Critical, move || {
                    let t = std::time::Instant::now();
                    cell.compute(&reader);
                    if let Some(timings) = timings {
                        timings.lock().unwrap().push((t.elapsed(), format!("{} {:?}", cell.kind, cell.key)));
                    }
                }));
            }
            for rx in rxs {
                let _ = scheduler.try_wait(rx, "fst plan");
            }
        }
        if profiling {
            let timings = timings.lock().unwrap();
            let sum: std::time::Duration = timings.iter().map(|(d, _)| *d).sum();
            let (max_d, max_name) = timings.iter().max_by_key(|(d, _)| *d)
                .map(|(d, n)| (*d, n.clone())).unwrap_or_default();
            self.report.wave_profile.push((n, t_wave.elapsed(), sum, max_name, max_d));
        }
    }
}

/// The distinct shard dictionaries among `segments` for `field`, with
/// whether their words may reach the suffix cap. Empty on a v3 index.
fn dictionaries<'a>(segments: &[&'a SegmentReader], field: Field) -> Vec<(&'a DictionaryField, bool)> {
    let mut out: Vec<(&DictionaryField, bool)> = Vec::new();
    for seg in segments {
        let Some(f) = seg.sfx_dictionary_field(field) else { continue };
        if out.iter().any(|(g, _)| std::ptr::eq(*g, f)) {
            continue;
        }
        let long = f.termtexts_reader().is_none_or(|t| t.may_have_long_words());
        out.push((f, long));
    }
    out
}

fn report(kind: &str, query: &str, r: &PlanReport) {
    if super::profile::enabled() {
        eprintln!("  [plan] {kind} {query:?}: {} waves, {} cells computed, {} held, wall {:.1}ms",
            r.waves, r.cells_computed, r.cells_held, r.wall.as_secs_f64() * 1e3);
        for (i, (n, wall, sum, max_name, max_d)) in r.wave_profile.iter().enumerate() {
            eprintln!("    wave {i}: {n} cells, wall {:.1}ms, CPU sum {:.1}ms, slowest {:.1}ms {max_name}",
                wall.as_secs_f64() * 1e3, sum.as_secs_f64() * 1e3, max_d.as_secs_f64() * 1e3);
        }
    }
}

/// Plan a contains query over the dictionaries of `segments`. Nothing
/// happens without a dictionary, with one segment, or under `V3_PLAN=0`.
pub fn plan_contains(segments: &[&SegmentReader], field: Field, query: &str, anchor_start: bool, strict_separators: bool) -> Option<PlanReport> {
    if segments.len() <= 1 || !plan_enabled() {
        return None;
    }
    let dicts = dictionaries(segments, field);
    if dicts.is_empty() {
        return None;
    }
    let effective = super::orchestrator::effective_query(query, strict_separators)?;
    let readers: Vec<SfxFileReaderV3> = dicts.iter().map(|(f, _)| f.sfx_reader().clone()).collect();
    let literals = dicts.iter().enumerate()
        .map(|(i, (_, long))| LiteralJob::new(i, &effective, anchor_start, strict_separators, *long))
        .collect();
    let r = Planner { readers, literals, fuzzies: Vec::new(), report: PlanReport::default() }.run();
    report("contains", query, &r);
    Some(r)
}

/// Plan a fuzzy query (see `plan_contains`).
pub fn plan_fuzzy(segments: &[&SegmentReader], field: Field, query: &str, distance: u8, strict_separators: bool) -> Option<PlanReport> {
    if segments.len() <= 1 || !plan_enabled() || distance > 3 {
        return None;
    }
    let dicts = dictionaries(segments, field);
    if dicts.is_empty() {
        return None;
    }
    let effective = super::orchestrator::effective_query(query, strict_separators)?;
    let readers: Vec<SfxFileReaderV3> = dicts.iter().map(|(f, _)| f.sfx_reader().clone()).collect();
    let mut literals = Vec::new();
    let mut fuzzies = Vec::new();
    for (i, (_, long)) in dicts.iter().enumerate() {
        if distance == 0 {
            literals.push(LiteralJob::new(i, &effective, false, strict_separators, *long));
        } else {
            fuzzies.push(FuzzyJob { dict: i, query: effective.clone(), distance, strict: strict_separators, may_have_long_words: *long, stage: 0 });
        }
    }
    let r = Planner { readers, literals, fuzzies, report: PlanReport::default() }.run();
    report("fuzzy", query, &r);
    Some(r)
}

/// Plan a regex query: its required literals (see `plan_contains`).
pub fn plan_regex(segments: &[&SegmentReader], field: Field, pattern: &str) -> Option<PlanReport> {
    if segments.len() <= 1 || !plan_enabled() {
        return None;
    }
    let dicts = dictionaries(segments, field);
    if dicts.is_empty() {
        return None;
    }
    let plan = super::regex_verified::plan(pattern)?;
    if plan.literals.is_empty() {
        return None;
    }
    let readers: Vec<SfxFileReaderV3> = dicts.iter().map(|(f, _)| f.sfx_reader().clone()).collect();
    let mut literals = Vec::new();
    for (i, (_, long)) in dicts.iter().enumerate() {
        for lit in &plan.literals {
            literals.push(LiteralJob::new(i, lit, false, true, *long));
        }
    }
    let r = Planner { readers, literals, fuzzies: Vec::new(), report: PlanReport::default() }.run();
    report("regex", pattern, &r);
    Some(r)
}
