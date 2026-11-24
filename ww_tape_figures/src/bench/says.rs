//! The numbers the prose of "Sizes and speeds" may write, so that a rerun of the benches updates them or fails:
//!
//! - pins: numbers the page computes from the reports (a range, a margin, a count), as the page writes them; each
//!   must appear in the page;
//! - named: every number the reports name, their cells as the page shows them and the numbers in their titles,
//!   text cells and settings, each cell also rounded to fewer significant digits (two at least) and in millions;
//! - given: phrases with numbers that are no measurement, each with its reason.
//!
//! [Says::check] takes the page and lists every number of its prose that none of them holds, and every pin it lacks.

use super::{Reports, grouped, row, rows, set_rows, shown, table, text, value};
use anyhow::{Context, ensure};
use std::collections::BTreeSet;
use ww_tape_bench_report::{Cell, Report, Table};

/// Phrases with numbers the page writes that are no measurement, and why.
const GIVEN: [(&str, &str); 3] = [
    (
        "60000/1001",
        "the frame rate of the video frame workloads (workloads.rs)",
    ),
    ("64-bit", "the width of a raw tick"),
    (
        "12-bit",
        "the converter of the ADC data sets (ww_tape_codecs_benchmarks data.rs)",
    ),
];

/// What the page's prose may write, from the reports.
pub struct Says {
    /// Numbers the page computes from the reports, as it writes them, and what each is.
    pub pins: Vec<(String, String)>,
    /// Every number the reports name.
    pub named: BTreeSet<String>,
}

/// A small count as a word, as the page writes it.
fn word(n: usize) -> anyhow::Result<&'static str> {
    [
        "no", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine",
    ]
    .get(n)
    .copied()
    .with_context(|| format!("{n} as a word"))
}

/// The numbers in `text`: digits with a decimal point or groups of three split by spaces, not part of a name such as
/// `i16`, `x86_64` or `AVX-512`.
fn numbers(text: &str) -> Vec<String> {
    let c: Vec<char> = text.chars().collect();
    let digit = |i: usize| c.get(i).is_some_and(char::is_ascii_digit);
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        if !digit(i) {
            i += 1;
            continue;
        }
        let named = i.checked_sub(1).and_then(|p| c.get(p)).is_some_and(|p| {
            p.is_alphanumeric()
                || *p == '_'
                || (*p == '-'
                    && i.checked_sub(2)
                        .and_then(|q| c.get(q))
                        .is_some_and(|q| q.is_alphanumeric()))
        });
        let start = i;
        let mut grouped_ok = true;
        loop {
            while digit(i) {
                i += 1;
            }
            if c.get(i) == Some(&'.') && digit(i + 1) {
                i += 1;
                grouped_ok = false;
                continue;
            }
            // a group of three after a space, when the number so far is one to three digits or groups
            let three = (1..=3).all(|k| digit(i + k)) && !digit(i + 4);
            if grouped_ok && c.get(i) == Some(&' ') && three {
                i += 4;
                continue;
            }
            break;
        }
        if !named {
            out.push(
                c.get(start..i)
                    .map(|s| s.iter().collect())
                    .unwrap_or_default(),
            );
        }
    }
    out
}

/// The prose of a page: no front matter, code blocks, includes, code spans, link targets or HTML tags.
fn prose(page: &str) -> String {
    let mut out = String::new();
    let mut fence = false;
    let mut front = page.starts_with("---");
    for (n, line) in page.lines().enumerate() {
        if front {
            if n > 0 && line == "---" {
                front = false;
            }
            continue;
        }
        if line.trim_start().starts_with("```") {
            fence = !fence;
            continue;
        }
        if fence || line.trim_start().starts_with("--8<--") {
            continue;
        }
        let mut keep = String::new();
        let (mut code, mut tag, mut target) = (false, false, false);
        let mut prev = ' ';
        for ch in line.chars() {
            match ch {
                '`' => code = !code,
                '<' if !code => tag = true,
                '>' if tag => tag = false,
                '(' if prev == ']' && !code => target = true,
                ')' if target => target = false,
                _ if code || tag || target => {}
                _ => keep.push(ch),
            }
            prev = ch;
        }
        out.push_str(&keep);
        out.push('\n');
    }
    out
}

impl Says {
    /// What `page` writes that the reports don't hold: numbers of its prose none of [Says] names, and pins it lacks.
    pub fn check(&self, page: &str) -> Vec<String> {
        let mut text = prose(page).split_whitespace().collect::<Vec<_>>().join(" ");
        let mut problems = Vec::new();
        for (what, shown) in &self.pins {
            if text.contains(shown.as_str()) {
                text = text.replace(shown.as_str(), " ");
            } else {
                problems.push(format!("the page lacks \"{shown}\": {what}"));
            }
        }
        for (phrase, _) in GIVEN {
            text = text.replace(phrase, " ");
        }
        for n in numbers(&text) {
            if !self.named.contains(&n) {
                problems.push(format!(
                    "\"{n}\" is in no report: write it from one, or as a pin of says.rs"
                ));
            }
        }
        problems
    }
}

/// Every number `report` names: cells as the page shows them, and the numbers in its titles, text and settings.
fn named(report: &Report, out: &mut BTreeSet<String>) {
    let mut rounds = Vec::new();
    let mut add = |s: &str| {
        for n in numbers(s) {
            if !n.contains('.') && n.len() >= 5 && !n.contains(' ') {
                out.insert(grouped(&n));
            }
            out.insert(n);
        }
    };
    for (k, v) in &report.settings {
        add(k);
        add(v);
    }
    add(&report.machine.cpus.to_string());
    for t in &report.tables {
        add(&t.title);
        for c in &t.columns {
            add(c);
        }
        for c in t.rows.iter().flatten() {
            match c {
                Cell::Text(s) => add(s),
                c @ Cell::Num { .. } => {
                    let shown = shown(c);
                    add(&shown);
                    if let Some(v) = c.value() {
                        // a whole value shown with decimals, such as 20.000 raw bytes, reads as 20 in prose
                        if v.fract() == 0.0 {
                            add(&format!("{v:.0}"));
                        }
                        rounds.extend(rounded(v, &shown));
                    }
                }
            }
        }
    }
    out.extend(rounds);
}

/// `v` rounded to `digits` significant digits, as the page writes it: 1.1, 870, 2100, 1 000 000.
fn sig(v: f64, digits: i32) -> String {
    if v == 0.0 {
        return "0".to_owned();
    }
    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        reason = "the exponent of a finite f64 fits an i32"
    )]
    let exp = v.abs().log10().floor() as i32;
    let decimals = digits - 1 - exp;
    if let Ok(d) = usize::try_from(decimals)
        && d > 0
    {
        format!("{v:.d$}")
    } else {
        let unit = 10f64.powi(-decimals);
        let whole = format!("{:.0}", (v / unit).round() * unit);
        if whole.trim_start_matches('-').len() >= 5 {
            grouped(&whole)
        } else {
            whole
        }
    }
}

/// The forms of a measured `v` the prose may round it to: two significant digits up to `shown`'s, and in millions.
fn rounded(v: f64, shown: &str) -> Vec<String> {
    let digits = shown.chars().filter(char::is_ascii_digit).count();
    let digits = i32::try_from(digits).unwrap_or(i32::MAX);
    let mut out: Vec<String> = (2..digits).map(|d| sig(v, d)).collect();
    if v.abs() >= 1e6 {
        out.extend((2..=3).map(|d| sig(v / 1e6, d)));
    }
    out
}

/// `lo to hi` of `v`, each to two significant digits.
fn range(v: &[f64]) -> anyhow::Result<String> {
    ensure!(!v.is_empty(), "a range of no values");
    let lo = v.iter().copied().fold(f64::MAX, f64::min);
    let hi = v.iter().copied().fold(f64::MIN, f64::max);
    Ok(format!("{} to {}", sig(lo, 2), sig(hi, 2)))
}

/// The value of `column` in the row `keys` of `t`.
fn at(t: &Table, keys: &[&str], column: &str) -> anyhow::Result<f64> {
    value(t, row(t, keys)?, column)
}

/// What the prose of "Sizes and speeds" may write, from `r`.
pub fn says(r: &Reports) -> anyhow::Result<Says> {
    let mut named_ = BTreeSet::new();
    for report in [
        &r.sample_codecs,
        &r.event_times,
        &r.summaries,
        &r.chunks,
        &r.tracks,
        &r.drive,
        &r.memory,
        &r.rf_core,
        &r.rf_chain,
    ] {
        named(report, &mut named_);
    }
    named_.insert(ww_tape::bitpack::GROUP.to_string());
    let mut pins = Vec::new();
    drive(r, &mut pins)?;
    samples(r, &mut pins)?;
    events(r, &mut pins)?;
    writer(r, &mut pins, &mut named_)?;
    rf(r)?;
    Ok(Says {
        pins,
        named: named_,
    })
}

fn drive(r: &Reports, pins: &mut Vec<(String, String)>) -> anyhow::Result<()> {
    let t = table(&r.drive, "sizes")?;
    let total = at(t, &["the recording"], "Bytes")?;
    pins.push((
        "the drive's hour in MiB".to_owned(),
        format!("{:.0} MiB", total / f64::from(1 << 20)),
    ));
    Ok(())
}

fn samples(r: &Reports, pins: &mut Vec<(String, String)>) -> anyhow::Result<()> {
    let t = table(&r.sample_codecs, "codecs")?;
    let sets = super::groups(t)?;
    let ratio = |row: &Vec<Cell>| value(t, row, "Ratio");
    let decode = |row: &Vec<Cell>| value(t, row, "Decode ns per value");
    let (mut alp_margin, mut slower) = (Vec::new(), Vec::new());
    let (mut xor_wins, mut pair_best, mut pair_short) = (Vec::new(), 0, Vec::new());
    for set in &sets {
        let s = set_rows(t, set)?;
        let best = ratio(s.alp)?.max(ratio(s.best)?);
        ensure!(
            ratio(s.fixed)? > best,
            "the page says fixed point wins on every set; not on {set}"
        );
        if set.contains("× 0.1") {
            ensure!(
                ratio(s.alp)? > ratio(s.best)?,
                "the page says ALP wins on {set}"
            );
            alp_margin.push(ratio(s.alp)? / ratio(s.best)?);
        }
        let mut fastest = decode(s.fixed)?.min(decode(s.alp)?);
        for codec in ["FLOAT_DELTA", "FLOAT_DELTA2"] {
            fastest = fastest.min(at(t, &[set, codec], "Decode ns per value")?);
        }
        for codec in ["GORILLA", "CHIMP"] {
            slower.push(at(t, &[set, codec], "Decode ns per value")? / fastest);
        }
        let others = rows(t, &[set])?
            .into_iter()
            .filter(|row| text(row, 1).is_ok_and(|c| c != "GORILLA" && c != "CHIMP"))
            .filter(|row| text(row, 1).is_ok_and(|c| !c.starts_with("DELTA of")))
            .map(ratio)
            .collect::<anyhow::Result<Vec<f64>>>()?;
        let not_xor = others.iter().copied().fold(0.0, f64::max);
        let xor = ["GORILLA", "CHIMP"]
            .into_iter()
            .map(|c| at(t, &[set, c], "Ratio"))
            .collect::<anyhow::Result<Vec<f64>>>()?
            .into_iter()
            .fold(0.0, f64::max);
        if xor > not_xor {
            xor_wins.push((set.clone(), (xor / not_xor - 1.0) * 100.0));
        }
        let pair = at(t, &[set, "ALP"], "Ratio")?.max(at(t, &[set, "FLOAT_DELTA"], "Ratio")?);
        if pair >= best {
            pair_best += 1;
        } else {
            pair_short.push((1.0 - pair / best) * 100.0);
        }
    }
    pins.push((
        "ALP's ratio over the best other float codec on the sets of ADC × 0.1".to_owned(),
        format!("{} times", range(&alp_margin)?),
    ));
    pins.push((
        "the sets where an XOR codec has the best float ratio".to_owned(),
        format!("on {} sets", word(xor_wins.len())?),
    ));
    for (set, margin) in &xor_wins {
        pins.push((
            format!("the XOR codec's margin over the other float codecs on {set}"),
            format!("{margin:.0} % on"),
        ));
    }
    pins.push((
        "the XOR codecs' decode time over the fastest bit-packed codec's, per set".to_owned(),
        format!("{} times slower", range(&slower)?),
    ));
    pins.push((
        "the sets where ALP or FLOAT_DELTA, whichever is smaller, is the best float codec"
            .to_owned(),
        format!("{} of the {} sets", word(pair_best)?, word(sets.len())?),
    ));
    pins.push((
        "how far ALP or FLOAT_DELTA falls behind the best float codec on the other sets".to_owned(),
        format!(
            "within {:.0} %",
            pair_short.iter().copied().fold(0.0, f64::max)
        ),
    ));
    let seconds: f64 = r.sample_codecs.tables.iter().map(|t| t.seconds).sum();
    pins.push((
        "the sample codec bench's run time".to_owned(),
        format!("about {seconds:.0} s"),
    ));
    Ok(())
}

fn events(r: &Reports, pins: &mut Vec<(String, String)>) -> anyhow::Result<()> {
    let e = &r.event_times;
    let t = table(e, "codecs")?;
    let find = "First at or after a tick ns";
    let bytes = "Bytes per event";
    let (mut linear, mut varints, mut ef_faster, mut losses) = (vec![], vec![], 0, vec![]);
    for w in super::groups(t)? {
        let l = at(t, &[&w, "ww_tape linear groups"], find)?;
        linear.push(l);
        varints.push(at(t, &[&w, "delta-of-delta varints"], find)?);
        if at(t, &[&w, "Elias-Fano"], find)? < l {
            ef_faster += 1;
        }
        if at(t, &[&w, "ww_tape linear groups"], bytes)?
            > at(t, &[&w, "delta-of-delta varints"], bytes)?
        {
            losses.push(w);
        }
    }
    pins.push((
        "the time the linear groups take to find a tick, over the workloads".to_owned(),
        format!("{} ns to find", range(&linear)?),
    ));
    pins.push((
        "the time the varints take to find a tick, over the workloads".to_owned(),
        format!("takes {} ns.", range(&varints)?),
    ));
    pins.push((
        "the workloads where Elias-Fano finds a tick faster than the linear groups".to_owned(),
        format!("on {} workloads", word(ef_faster)?),
    ));
    ensure!(
        losses
            .iter()
            .all(|w| w == "bursty traffic" || w.starts_with("a system journal")),
        "the page says the linear groups are larger than the varints only on bursty traffic and the journal: {losses:?}"
    );
    let bursty = "bursty traffic";
    pins.push((
        "what random access costs on bursty traffic: the linear groups' bytes over the varints'"
            .to_owned(),
        format!(
            "{} bytes per event",
            sig(
                at(t, &[bursty, "ww_tape linear groups"], bytes)?
                    - at(t, &[bursty, "delta-of-delta varints"], bytes)?,
                2
            )
        ),
    ));
    let setting = |name: &str| -> anyhow::Result<f64> {
        e.setting(name)
            .with_context(|| format!("setting {name} of bench event_times"))?
            .parse::<f64>()
            .with_context(|| format!("setting {name} of bench event_times"))
    };
    pins.push((
        "the events of each generated workload".to_owned(),
        format!("{:.0} million events", setting("events")? / 1e6),
    ));
    pins.push((
        "the video frames of the frame clock workloads".to_owned(),
        format!("{:.0} million video frames", setting("frames")? / 1e6),
    ));
    pins.push((
        "the entries of the system journal, in millions".to_owned(),
        format!(
            "{} million entries",
            sig(setting("journal entries")? / 1e6, 2)
        ),
    ));
    pins.push((
        "the days of the system journal".to_owned(),
        format!("{:.0} days", setting("journal days")?),
    ));
    Ok(())
}

fn writer(
    r: &Reports,
    pins: &mut Vec<(String, String)>,
    named: &mut BTreeSet<String>,
) -> anyhow::Result<()> {
    let t = table(&r.chunks, "writer")?;
    for row in &t.rows {
        named.insert(sig(value(t, row, "ns per chunk")? / 1000.0, 2));
    }
    let kib = r
        .chunks
        .setting("chunk KiB")
        .context("setting chunk KiB of bench chunks")?
        .parse::<u32>()?;
    ensure!(kib % 1024 == 0, "the page writes the chunk in MiB");
    named.insert((kib / 1024).to_string());
    let ns = |what: &str| -> anyhow::Result<f64> {
        let rows: Vec<&Vec<Cell>> = t
            .rows
            .iter()
            .filter(|r| text(r, 0).is_ok_and(|c| c.starts_with(what)))
            .collect();
        ensure!(rows.len() == 1, "one writer row {what}");
        value(t, rows.first().context("a row")?, "ns per chunk")
    };
    pins.push((
        "what the writer spends on a chunk besides the content hash".to_owned(),
        format!(
            "{} µs per MiB",
            sig(
                (ns("`TapeWriter::write_chunk` (columns)")? - ns("`ItemHash::of`")?)
                    / 1000.0
                    / f64::from(kib / 1024),
                2
            )
        ),
    ));
    Ok(())
}

/// The claims of the radio capture's section: `DELTA` saves little over `PACKED12_LSB`, and the codec takes more of
/// the writer's time than `write_chunk`.
fn rf(r: &Reports) -> anyhow::Result<()> {
    let t = table(&r.rf_core, "writer")?;
    let bytes = |codec: &str| at(t, &[codec], "Bytes per sample");
    let (packed, delta) = (bytes("`PACKED12_LSB`")?, bytes("`DELTA`")?);
    ensure!(
        delta < packed && delta > packed * 0.9,
        "the page says DELTA saves little over PACKED12_LSB: {delta} against {packed} bytes per sample"
    );
    for codec in ["`PACKED12_LSB`", "`DELTA`"] {
        let (coding, write) = (
            at(t, &[codec], "Codec ms/s")?,
            at(t, &[codec], "`write_chunk` ms/s")?,
        );
        ensure!(
            coding > write,
            "the page says the codec takes more than write_chunk: {codec} {coding} against {write} ms/s"
        );
    }
    Ok(())
}

/// The pins of `pins` that the prose of `page` lacks, each as a problem to report.
pub fn missing(pins: &[(String, String)], page: &str) -> Vec<String> {
    let text = prose(page).split_whitespace().collect::<Vec<_>>().join(" ");
    pins.iter()
        .filter(|(_, shown)| !text.contains(shown.as_str()))
        .map(|(what, shown)| format!("the page lacks \"{shown}\": {what}"))
        .collect()
}

/// The value of `column` in the row of `t` for `writer` with `tracks` tracks.
fn at_tracks(t: &Table, writer: &str, tracks: f64, column: &str) -> anyhow::Result<f64> {
    let mut found = Vec::new();
    for r in rows(t, &[writer])? {
        if value(t, r, "Tracks")? == tracks {
            found.push(r);
        }
    }
    ensure!(
        found.len() == 1,
        "one row {writer} with {tracks} tracks in table {}",
        t.id
    );
    value(t, found.first().context("a row")?, column)
}

/// The numbers the prose of "Writing from many threads" computes from the handles report, as it writes them, and
/// what each is. The page holds each one ([Says::pins]).
pub fn handles(r: &Reports) -> anyhow::Result<Vec<(String, String)>> {
    let events = table(&r.handles, "events")?;
    let iq = table(&r.handles, "iq")?;
    let tracks_max = events
        .rows
        .iter()
        .map(|row| value(events, row, "Tracks"))
        .collect::<anyhow::Result<Vec<f64>>>()?
        .into_iter()
        .fold(0.0f64, f64::max);
    let percent = |v: f64| format!("{} %", sig(v * 100.0, 2));
    let one = at_tracks(events, "mux", 1.0, "vs direct")?;
    let one_by_one = at_tracks(events, "mux, one by one", 1.0, "vs direct")?;
    let many = at_tracks(events, "mux", tracks_max, "vs direct")?;
    let iq_many = at_tracks(iq, "mux", tracks_max, "vs direct")?;
    ensure!(
        one < 1.0 && one_by_one < one && many > 1.0 && iq_many > 1.0,
        "the page says one track through the mux is slower than direct, one by one slower still, and many tracks \
         faster: {one}, {one_by_one}, {many}, {iq_many}"
    );
    Ok(vec![
        (
            "one events track through the mux against direct".to_owned(),
            percent(one),
        ),
        (
            "one events track pushed one by one against direct".to_owned(),
            percent(one_by_one),
        ),
        (
            "the most events tracks through the mux against direct".to_owned(),
            format!("{} times as many events", sig(many, 2)),
        ),
        (
            "the most IQ tracks through the mux against direct".to_owned(),
            format!("{} times as many samples", sig(iq_many, 2)),
        ),
        (
            "the bytes the mux writes a second with the most IQ tracks".to_owned(),
            format!(
                "{} GB/s",
                sig(
                    at_tracks(iq, "mux", tracks_max, "MB/s written")? / 1000.0,
                    2
                )
            ),
        ),
        (
            "sends that waited for room with the most IQ tracks".to_owned(),
            format!(
                "{} sends waited",
                grouped(&format!(
                    "{:.0}",
                    at_tracks(iq, "mux", tracks_max, "Waits")?
                ))
            ),
        ),
        (
            "the most tracks of the handles bench".to_owned(),
            format!("{tracks_max:.0} tracks"),
        ),
    ])
}

/// The numbers the prose of "When bytes go bad" computes from the read_check report, as it writes them, and what
/// each is. The page holds each one ([missing]).
pub fn damage(r: &Reports) -> anyhow::Result<Vec<(String, String)>> {
    let t = table(&r.read_check, "read")?;
    let ns = |block: &str, check: &str| value(t, row(t, &[block, check])?, "ns per block");
    let share = |block: &str| -> anyhow::Result<f64> {
        Ok(ns(block, "not checked")? / ns(block, "checked")?)
    };
    let (chunk, records) = (share("chunk")?, share("records")?);
    ensure!(
        chunk < 1.0 && records < 1.0,
        "the page says an unchecked read is faster: {chunk}, {records}"
    );
    let percent = |v: f64| format!("{} %", sig(v * 100.0, 2));
    Ok(vec![
        (
            "an unchecked read of a chunk against a checked one".to_owned(),
            percent(chunk),
        ),
        (
            "an unchecked read of a records block against a checked one".to_owned(),
            percent(records),
        ),
    ])
}

/// The numbers the prose of "Fewer checks, sealed later" computes from the levels report, as it writes them, and what
/// each is. The page holds each one ([missing]).
pub fn integrity(r: &Reports) -> anyhow::Result<Vec<(String, String)>> {
    let percent = |v: f64| format!("{} %", sig(v * 100.0, 2));
    let w = table(&r.levels, "write")?;
    let s = table(&r.levels, "seal")?;
    let write = |level: &str| value(w, row(w, &["series 1 MiB", level])?, "ns per chunk");
    let full = write("Full")?;
    let (crc_only, none) = (write("CrcOnly")? / full, write("None")? / full);
    ensure!(
        none < crc_only && crc_only < 1.0,
        "the page says each lower level writes faster: {crc_only}, {none}"
    );
    let seal = value(s, row(s, &["series 1 MiB", "None"])?, "ns per chunk")? / full;
    Ok(vec![
        (
            "a 1 MiB series chunk written at CrcOnly against Full".to_owned(),
            percent(crc_only),
        ),
        (
            "a 1 MiB series chunk written at None against Full".to_owned(),
            percent(none),
        ),
        (
            "the seal of a 1 MiB series chunk of None against writing it at Full".to_owned(),
            percent(seal),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_of_prose() {
        assert_eq!(
            numbers(
                "i16 at 10 671 MB/s, 1.107 bytes, AVX-512, x86_64, ±300 ns, 3.3/4096 and 2 raw bytes"
            ),
            ["10 671", "1.107", "300", "3.3", "4096", "2"]
        );
    }

    #[test]
    fn rounding_keeps_two_significant_digits() {
        assert_eq!(sig(1.107, 2), "1.1");
        assert_eq!(sig(868.0, 2), "870");
        assert_eq!(sig(2133.0, 2), "2100");
        assert_eq!(sig(0.0142, 2), "0.014");
        assert_eq!(sig(5_080_816.0, 2), "5 100 000");
        assert_eq!(
            rounded(5_080_816.0, "5 080 816"),
            [
                "5 100 000",
                "5 080 000",
                "5 081 000",
                "5 080 800",
                "5 080 820",
                "5.1",
                "5.08"
            ]
        );
        assert_eq!(rounded(86.41, "86.41"), ["86", "86.4"]);
    }

    #[test]
    fn prose_leaves_out_code_includes_and_tags() {
        let page = "---\nlevel: 1\n---\n\nA `u64` of [8](x2.md) <b class=\"c3\">9</b>\n```sh\njust jobs=4\n```\n--8<-- \"a5.txt\"\n";
        assert_eq!(numbers(&prose(page)), ["8", "9"]);
    }
}
