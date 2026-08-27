//! Shared SDK benchmark presentation, adapted from Taskvisor's benchmark report.
//!
//! Every card reports a semantic unit and exact measured boundary. Saved estimates
//! are accepted only when they changed during this invocation.

#![allow(dead_code)]

use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use anstream::{AutoStream, ColorChoice};
use anstyle::{AnsiColor, Style};
use serde::Deserialize;

const REPORT_WIDTH: usize = 92;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Lifecycle,
    Intake,
    Policy,
    Query,
}

impl Scope {
    const fn badge(self) -> &'static str {
        match self {
            Self::Lifecycle => "FULL LIFECYCLE",
            Self::Policy => "POLICY DECISION",
            Self::Intake => "INTAKE ONLY",
            Self::Query => "QUERY",
        }
    }

    const fn color(self) -> AnsiColor {
        match self {
            Self::Lifecycle => AnsiColor::BrightGreen,
            Self::Policy => AnsiColor::BrightYellow,
            Self::Query => AnsiColor::BrightMagenta,
            Self::Intake => AnsiColor::BrightBlue,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CaseFamily {
    pub group_id: &'static str,
    pub title: &'static str,
    pub scope: Scope,
    pub unit_singular: &'static str,
    pub unit_plural: &'static str,
    pub boundary: &'static str,
    pub outside: &'static str,
    pub interpretation: Interpretation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interpretation {
    ManagedTaskLifecycle,
    Neutral,
}

impl CaseFamily {
    pub const fn lifecycle(
        group_id: &'static str,
        title: &'static str,
        unit_singular: &'static str,
        unit_plural: &'static str,
        boundary: &'static str,
        outside: &'static str,
    ) -> Self {
        Self {
            group_id,
            title,
            scope: Scope::Lifecycle,
            unit_singular,
            unit_plural,
            boundary,
            outside,
            interpretation: Interpretation::ManagedTaskLifecycle,
        }
    }

    pub const fn intake(
        group_id: &'static str,
        title: &'static str,
        unit_singular: &'static str,
        unit_plural: &'static str,
        boundary: &'static str,
        outside: &'static str,
    ) -> Self {
        Self {
            group_id,
            title,
            scope: Scope::Intake,
            unit_singular,
            unit_plural,
            boundary,
            outside,
            interpretation: Interpretation::Neutral,
        }
    }

    pub const fn policy(
        group_id: &'static str,
        title: &'static str,
        unit_singular: &'static str,
        unit_plural: &'static str,
        boundary: &'static str,
        outside: &'static str,
    ) -> Self {
        Self {
            group_id,
            title,
            scope: Scope::Policy,
            unit_singular,
            unit_plural,
            boundary,
            outside,
            interpretation: Interpretation::Neutral,
        }
    }

    pub const fn query(
        group_id: &'static str,
        title: &'static str,
        unit_singular: &'static str,
        unit_plural: &'static str,
        boundary: &'static str,
        outside: &'static str,
    ) -> Self {
        Self {
            group_id,
            title,
            scope: Scope::Query,
            unit_singular,
            unit_plural,
            boundary,
            outside,
            interpretation: Interpretation::Neutral,
        }
    }

    pub const fn without_lifecycle_interpretation(mut self) -> Self {
        self.interpretation = Interpretation::Neutral;
        self
    }
}

#[derive(Clone, Debug)]
struct RecordedCase {
    full_id: String,
    family: CaseFamily,
}

static RECORDED_CASES: OnceLock<Mutex<Vec<RecordedCase>>> = OnceLock::new();

pub fn record_case(family: CaseFamily, function_id: &str, value_str: Option<String>) {
    let full_id = match value_str {
        Some(value) => format!("{}/{function_id}/{value}", family.group_id),
        None => format!("{}/{function_id}", family.group_id),
    };
    let cases = RECORDED_CASES.get_or_init(|| Mutex::new(Vec::new()));
    let mut cases = cases.lock().expect("benchmark result recorder is poisoned");
    if !cases.iter().any(|case| case.full_id == full_id) {
        cases.push(RecordedCase { full_id, family });
    }
}

pub fn print_suite_header(suite: &str) {
    if !statistical_run_requested() {
        return;
    }
    static PRINTED: OnceLock<()> = OnceLock::new();
    PRINTED.get_or_init(|| {
        let logical_cpus = std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(1);
        let cpu = cpu_model();
        let revision = git_revision();
        let cyan = style(AnsiColor::BrightCyan, true);
        let dim = Style::new().dimmed();
        let mut out = output();
        let title = format!("SOLTI SDK BENCHMARK · {}", suite.to_uppercase());
        let platform = format!(
            "{} · {} · {logical_cpus} logical CPUs",
            display_os(std::env::consts::OS),
            std::env::consts::ARCH,
        );
        let build = revision.map_or_else(
            || format!("solti-sdk {}", env!("CARGO_PKG_VERSION")),
            |revision| format!("solti-sdk {} · {revision}", env!("CARGO_PKG_VERSION")),
        );

        writeln!(out).ok();
        write_header_top(&mut out, &title, cyan);
        if let Some(cpu) = cpu {
            write_header_row(&mut out, "CPU", &cpu, cyan);
        }
        write_header_row(&mut out, "Platform", &platform, cyan);
        write_header_row(&mut out, "Build", &build, cyan);
        write_header_row(&mut out, "Features", &enabled_features(), cyan);
        write_header_bottom(&mut out, cyan);
        writeln!(
            out,
            "{dim}MEASURED = Criterion estimates from this run{dim:#}"
        )
        .ok();
        writeln!(out).ok();
    });
}

fn write_header_top(out: &mut AutoStream<std::io::Stdout>, title: &str, accent: Style) {
    let fill = REPORT_WIDTH.saturating_sub(title.chars().count() + 5);
    writeln!(out, "{accent}╭─ {title} {}╮{accent:#}", "─".repeat(fill)).ok();
}

fn write_header_row(
    out: &mut AutoStream<std::io::Stdout>,
    label: &str,
    value: &str,
    accent: Style,
) {
    const LABEL_WIDTH: usize = 10;

    let inner_width = REPORT_WIDTH - 4;
    let value_width = inner_width - LABEL_WIDTH;
    for (index, line) in wrap_words(value, value_width).iter().enumerate() {
        let label = if index == 0 { label } else { "" };
        let label = format!("{label:<width$}", width = LABEL_WIDTH);
        let padding = inner_width.saturating_sub(label.chars().count() + line.chars().count());
        writeln!(
            out,
            "{accent}│{accent:#} {accent}{label}{accent:#}{line}{} {accent}│{accent:#}",
            " ".repeat(padding),
        )
        .ok();
    }
}

fn write_header_bottom(out: &mut AutoStream<std::io::Stdout>, accent: Style) {
    writeln!(out, "{accent}╰{}╯{accent:#}", "─".repeat(REPORT_WIDTH - 2),).ok();
}

fn display_os(os: &str) -> &str {
    match os {
        "linux" => "Linux",
        "macos" => "macOS",
        "windows" => "Windows",
        other => other,
    }
}

pub fn benchmark_main(suite: &'static str, run: fn()) {
    let roots = if statistical_run_requested() && !discard_baseline_requested() {
        criterion_roots()
    } else {
        Vec::new()
    };
    let saved_estimates = roots
        .iter()
        .flat_map(|root| snapshot_saved_estimates(root))
        .collect();
    run();
    criterion::Criterion::default()
        .configure_from_args()
        .final_summary();
    print_performance_snapshot(suite, &roots, &saved_estimates);
}

#[derive(Deserialize)]
struct SavedBenchmark {
    group_id: String,
    function_id: Option<String>,
    value_str: Option<String>,
    throughput: Option<HashMap<String, u64>>,
    full_id: String,
}

#[derive(Clone, Copy, Deserialize)]
struct ConfidenceInterval {
    confidence_level: f64,
    lower_bound: f64,
    upper_bound: f64,
}

#[derive(Clone, Copy, Deserialize)]
struct Estimate {
    confidence_interval: ConfidenceInterval,
    point_estimate: f64,
}

#[derive(Deserialize)]
struct Estimates {
    mean: Estimate,
    slope: Option<Estimate>,
}

struct Observation {
    case: RecordedCase,
    function_id: String,
    value_str: Option<String>,
    units: u64,
    time: Estimate,
}

struct ObservationGroup<'a> {
    family: CaseFamily,
    observations: Vec<&'a Observation>,
}

#[derive(PartialEq, Eq)]
struct SavedEstimateState {
    modified: SystemTime,
    bytes: Vec<u8>,
}

fn print_performance_snapshot(
    suite: &str,
    roots: &[PathBuf],
    saved_estimates: &HashMap<PathBuf, SavedEstimateState>,
) {
    if !statistical_run_requested() {
        return;
    }
    if discard_baseline_requested() {
        let mut out = output();
        writeln!(
            out,
            "\nNo Solti SDK snapshot: --discard-baseline does not save estimates."
        )
        .ok();
        return;
    }

    let cases = RECORDED_CASES
        .get()
        .map(|cases| {
            cases
                .lock()
                .expect("benchmark result recorder is poisoned")
                .clone()
        })
        .unwrap_or_default();
    if cases.is_empty() {
        return;
    }

    let mut observations = Vec::new();
    let mut result_roots = Vec::new();
    for case in cases {
        match load_observation_from_roots(roots, case, saved_estimates) {
            Ok((observation, root)) => {
                observations.push(observation);
                if !result_roots.contains(&root) {
                    result_roots.push(root);
                }
            }
            Err(error) => {
                let yellow = style(AnsiColor::BrightYellow, true);
                let mut out = output();
                writeln!(
                    out,
                    "{yellow}Solti SDK report skipped one case: {error}{yellow:#}"
                )
                .ok();
            }
        }
    }
    if observations.is_empty() {
        return;
    }
    let groups = group_observations(&observations);

    let cyan = style(AnsiColor::BrightCyan, true);
    let red = style(AnsiColor::BrightRed, true);
    let dim = Style::new().dimmed();
    let mut out = output();
    let title = format!("SOLTI SDK PERFORMANCE SNAPSHOT · {}", suite.to_uppercase());
    writeln!(out).ok();
    write_header_top(&mut out, &title, cyan);
    write_header_row(&mut out, "Results", &observations.len().to_string(), cyan);
    write_header_row(&mut out, "Groups", &groups.len().to_string(), cyan);
    write_header_row(
        &mut out,
        "Source",
        "absolute estimates from this benchmark invocation",
        cyan,
    );
    write_header_bottom(&mut out, cyan);
    writeln!(out).ok();

    for group in &groups {
        print_observation_group(&mut out, group);
    }

    let mut lifecycle_rates = Vec::new();
    for observation in &observations {
        if observation.case.family.interpretation == Interpretation::ManagedTaskLifecycle {
            lifecycle_rates.push(rate(observation.units, observation.time.point_estimate));
        }
    }

    writeln!(out, "{cyan}RUN SUMMARY{cyan:#}").ok();
    writeln!(out, "  {}", managed_lifecycle_summary(&lifecycle_rates)).ok();
    writeln!(out, "  Run status          all reported cases completed").ok();
    if noplot_requested() {
        writeln!(out, "  HTML report         disabled by --noplot").ok();
    } else {
        for root in &result_roots {
            let report_path = report_path_for_display(root);
            writeln!(
                out,
                "{red}  HTML report         {}{red:#}",
                report_path.display()
            )
            .ok();
        }
    }
    writeln!(
        out,
        "{dim}Compare results only after checking Boundary, Outside, Scope, runtime, and case parameters.{dim:#}"
    )
    .ok();
    writeln!(
        out,
        "{dim}Results describe this run on this host; they do not predict application capacity.{dim:#}"
    )
    .ok();
    writeln!(out).ok();
}

fn group_observations(observations: &[Observation]) -> Vec<ObservationGroup<'_>> {
    let mut groups: Vec<ObservationGroup<'_>> = Vec::new();
    for observation in observations {
        if let Some(group) = groups
            .iter_mut()
            .find(|group| group.family.group_id == observation.case.family.group_id)
        {
            group.observations.push(observation);
        } else {
            groups.push(ObservationGroup {
                family: observation.case.family,
                observations: vec![observation],
            });
        }
    }
    groups
}

fn load_observation(
    root: &Path,
    case: RecordedCase,
    saved_estimates: &HashMap<PathBuf, SavedEstimateState>,
) -> Result<Observation, String> {
    let mut candidates = Vec::new();
    collect_benchmark_files(root, &mut candidates).map_err(|error| error.to_string())?;
    let mut matched = None;
    for benchmark_path in candidates {
        let bytes = fs::read(&benchmark_path).map_err(|error| error.to_string())?;
        let benchmark: SavedBenchmark =
            serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        if benchmark.full_id == case.full_id {
            matched = Some((benchmark_path, benchmark));
            break;
        }
    }
    let (benchmark_path, benchmark) =
        matched.ok_or_else(|| format!("missing Criterion result for {}", case.full_id))?;
    if benchmark.group_id != case.family.group_id {
        return Err(format!("unexpected benchmark family for {}", case.full_id));
    }
    let units = benchmark
        .throughput
        .as_ref()
        .and_then(|throughput| throughput.get("Elements"))
        .copied()
        .ok_or_else(|| format!("missing Elements throughput for {}", case.full_id))?;
    let estimates_path = benchmark_path
        .parent()
        .expect("benchmark.json has a parent")
        .join("estimates.json");
    let current_estimate =
        saved_estimate_state(&estimates_path).map_err(|error| error.to_string())?;
    if saved_estimates
        .get(&estimates_path)
        .is_some_and(|saved| saved == &current_estimate)
    {
        return Err(format!("stale Criterion estimate for {}", case.full_id));
    }
    let estimates: Estimates =
        serde_json::from_slice(&current_estimate.bytes).map_err(|error| error.to_string())?;
    let time = estimates.slope.unwrap_or(estimates.mean);
    if units == 0
        || !time.point_estimate.is_finite()
        || time.point_estimate <= 0.0
        || !time.confidence_interval.lower_bound.is_finite()
        || time.confidence_interval.lower_bound <= 0.0
        || !time.confidence_interval.upper_bound.is_finite()
        || time.confidence_interval.upper_bound < time.confidence_interval.lower_bound
    {
        return Err(format!("invalid Criterion estimate for {}", case.full_id));
    }

    Ok(Observation {
        case,
        function_id: benchmark.function_id.unwrap_or_else(|| "case".to_owned()),
        value_str: benchmark.value_str,
        units,
        time,
    })
}

fn load_observation_from_roots(
    roots: &[PathBuf],
    case: RecordedCase,
    saved_estimates: &HashMap<PathBuf, SavedEstimateState>,
) -> Result<(Observation, PathBuf), String> {
    let mut found = None;
    let mut errors = Vec::new();
    for root in roots {
        match load_observation(root, case.clone(), saved_estimates) {
            Ok(observation) => {
                if found.is_some() {
                    return Err(format!(
                        "ambiguous fresh Criterion results in multiple directories for {}",
                        case.full_id
                    ));
                }
                found = Some((observation, root.clone()));
            }
            Err(error) => errors.push(format!("{}: {error}", root.display())),
        }
    }
    found.ok_or_else(|| errors.join("; "))
}

fn snapshot_saved_estimates(root: &Path) -> HashMap<PathBuf, SavedEstimateState> {
    let mut benchmark_files = Vec::new();
    if collect_benchmark_files(root, &mut benchmark_files).is_err() {
        return HashMap::new();
    }
    benchmark_files
        .into_iter()
        .filter_map(|benchmark_path| {
            let estimates_path = benchmark_path.parent()?.join("estimates.json");
            saved_estimate_state(&estimates_path)
                .ok()
                .map(|state| (estimates_path, state))
        })
        .collect()
}

fn saved_estimate_state(path: &Path) -> std::io::Result<SavedEstimateState> {
    Ok(SavedEstimateState {
        modified: fs::metadata(path)?.modified()?,
        bytes: fs::read(path)?,
    })
}

fn collect_benchmark_files(root: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if !root.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "new") {
                let benchmark = path.join("benchmark.json");
                if benchmark.is_file() {
                    files.push(benchmark);
                }
            } else {
                collect_benchmark_files(&path, files)?;
            }
        }
    }
    Ok(())
}

fn print_observation_group(out: &mut AutoStream<std::io::Stdout>, group: &ObservationGroup<'_>) {
    let family = group.family;
    let accent = style(family.scope.color(), true);
    let dim = Style::new().dimmed();

    writeln!(
        out,
        "{accent}┌─ ● MEASURED · {} · {}{accent:#}",
        family.scope.badge(),
        family.title,
    )
    .ok();
    writeln!(out, "{accent}│{accent:#}").ok();

    for (index, observation) in group.observations.iter().enumerate() {
        let is_last = index + 1 == group.observations.len();
        print_observation_result(out, observation, is_last);
        if !is_last {
            writeln!(out, "{accent}│{accent:#} {accent}│{accent:#}").ok();
        }
    }

    writeln!(out, "{accent}│{accent:#}").ok();
    write_wrapped_field(out, accent, "Boundary: ", family.boundary, None);
    write_wrapped_field(out, accent, "Outside:  ", family.outside, Some(dim));

    print_group_scope(out, family, accent, dim);
    writeln!(out, "{accent}└{}{accent:#}", "─".repeat(REPORT_WIDTH - 1),).ok();
    writeln!(out).ok();
}

fn print_group_scope(
    out: &mut AutoStream<std::io::Stdout>,
    family: CaseFamily,
    accent: Style,
    dim: Style,
) {
    writeln!(out, "{accent}│{accent:#}").ok();
    writeln!(
        out,
        "{accent}│{accent:#} {dim}◆ SCOPE · {}{dim:#}",
        scope_description(family),
    )
    .ok();
}

fn print_observation_result(
    out: &mut AutoStream<std::io::Stdout>,
    observation: &Observation,
    is_last: bool,
) {
    let family = observation.case.family;
    let accent = style(family.scope.color(), true);
    let branch = if is_last { "└─" } else { "├─" };
    let connector = if is_last { " " } else { "│" };
    let point_rate = rate(observation.units, observation.time.point_estimate);
    let low_rate = rate(
        observation.units,
        observation.time.confidence_interval.upper_bound,
    );
    let high_rate = rate(
        observation.units,
        observation.time.confidence_interval.lower_bound,
    );
    let unit_ns = observation.time.point_estimate / observation.units as f64;
    let details = observation_details(observation);

    writeln!(out, "{accent}│ {branch} {details}{accent:#}").ok();
    write_observation_line(
        out,
        accent,
        connector,
        &format!("{} {}/s", format_rate(point_rate), family.unit_plural),
        Some(accent),
    );
    let readable_rate = if family.scope == Scope::Lifecycle {
        format!(
            "{} {} each second across this measured lifecycle",
            format_count(point_rate),
            family.unit_plural,
        )
    } else {
        format!(
            "{} {} each second at this measured boundary",
            format_count(point_rate),
            family.unit_plural,
        )
    };
    write_observation_line(out, accent, connector, &format!("≈ {readable_rate}"), None);
    let cost_label = if observation.units > 1 {
        "amortized per"
    } else {
        "per"
    };
    write_observation_line(
        out,
        accent,
        connector,
        &format!(
            "{} {cost_label} {}",
            format_duration(unit_ns),
            family.unit_singular,
        ),
        None,
    );
    if observation.units > 1 {
        let unit_label =
            pluralize_for_count(family.unit_singular, family.unit_plural, observation.units);
        write_observation_line(
            out,
            accent,
            connector,
            &format!(
                "{} for the complete batch of {} {}",
                format_duration(observation.time.point_estimate),
                observation.units,
                unit_label,
            ),
            None,
        );
    }
    write_observation_line(
        out,
        accent,
        connector,
        &format!(
            "{:.0}% CI: {}–{} {}/s",
            observation.time.confidence_interval.confidence_level * 100.0,
            format_rate(low_rate),
            format_rate(high_rate),
            family.unit_plural,
        ),
        None,
    );
}

fn observation_details(observation: &Observation) -> String {
    observation.value_str.as_deref().map_or_else(
        || display_runtime(&observation.function_id),
        |value| {
            format!(
                "{} · {}",
                display_runtime(&observation.function_id),
                humanize(value)
            )
        },
    )
}

fn write_observation_line(
    out: &mut AutoStream<std::io::Stdout>,
    accent: Style,
    connector: &str,
    value: &str,
    value_style: Option<Style>,
) {
    let lines = wrap_words(value, REPORT_WIDTH.saturating_sub(6).max(20));
    for line in lines {
        let prefix = format!("{accent}│{accent:#} {accent}{connector}{accent:#}  ");
        if let Some(style) = value_style {
            writeln!(out, "{prefix}{style}{line}{style:#}").ok();
        } else {
            writeln!(out, "{prefix}{line}").ok();
        }
    }
}

fn scope_description(family: CaseFamily) -> String {
    if family.interpretation == Interpretation::ManagedTaskLifecycle {
        "COMPLETE MANAGED-TASK LIFECYCLE".to_owned()
    } else if family.scope == Scope::Lifecycle {
        format!(
            "COMPLETE LIFECYCLE · {}",
            family.unit_plural.to_ascii_uppercase()
        )
    } else {
        "OPERATION RATE, NOT COMPLETED-TASK THROUGHPUT".to_owned()
    }
}

fn managed_lifecycle_summary(rates: &[f64]) -> String {
    match rates {
        [] => "Managed lifecycle   not measured in this run".to_owned(),
        [rate] => format!(
            "Managed lifecycle   {} completed task lifecycles/s",
            format_rate(*rate),
        ),
        rates => format!(
            "Managed lifecycle   {} results; exact rates are shown in their groups",
            rates.len(),
        ),
    }
}

fn rate(units: u64, time_ns: f64) -> f64 {
    units as f64 * 1_000_000_000.0 / time_ns
}

fn format_rate(value: f64) -> String {
    if value >= 1_000_000_000.0 {
        format!("{:.3} G", value / 1_000_000_000.0)
    } else if value >= 1_000_000.0 {
        format!("{:.3} M", value / 1_000_000.0)
    } else if value >= 1_000.0 {
        format!("{:.3} K", value / 1_000.0)
    } else {
        format!("{value:.3}")
    }
}

fn format_count(value: f64) -> String {
    let rounded = value.round() as u64;
    let digits = rounded.to_string();
    let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(ch);
    }
    formatted
}

fn display_runtime(value: &str) -> String {
    match value {
        "current_thread" => "Tokio current-thread".to_owned(),
        "multi_thread" => "Tokio multi-thread · 4 workers".to_owned(),
        other => humanize(other),
    }
}

fn humanize(value: &str) -> String {
    value.replace('_', " ")
}

fn format_duration(ns: f64) -> String {
    if ns >= 1_000_000_000.0 {
        format!("{:.3} s", ns / 1_000_000_000.0)
    } else if ns >= 1_000_000.0 {
        format!("{:.3} ms", ns / 1_000_000.0)
    } else if ns >= 1_000.0 {
        format!("{:.3} µs", ns / 1_000.0)
    } else {
        format!("{ns:.3} ns")
    }
}

fn pluralize_for_count<'a>(singular: &'a str, plural: &'a str, count: u64) -> &'a str {
    if count == 1 { singular } else { plural }
}

fn write_wrapped_field(
    out: &mut AutoStream<std::io::Stdout>,
    accent: Style,
    label: &str,
    value: &str,
    value_style: Option<Style>,
) {
    let available = REPORT_WIDTH
        .saturating_sub(2 + label.chars().count())
        .max(20);
    let lines = wrap_words(value, available);
    for (index, line) in lines.iter().enumerate() {
        let prefix = if index == 0 {
            format!("{accent}│{accent:#} {label}")
        } else {
            format!("{accent}│{accent:#} {}", " ".repeat(label.chars().count()))
        };
        if let Some(style) = value_style {
            writeln!(out, "{prefix}{style}{line}{style:#}").ok();
        } else {
            writeln!(out, "{prefix}{line}").ok();
        }
    }
}

fn wrap_words(value: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in value.split_whitespace() {
        let separator = usize::from(!line.is_empty());
        if !line.is_empty() && line.chars().count() + separator + word.chars().count() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

fn style(color: AnsiColor, bold: bool) -> Style {
    let style = Style::new().fg_color(Some(color.into()));
    if bold { style.bold() } else { style }
}

fn output() -> AutoStream<std::io::Stdout> {
    AutoStream::new(std::io::stdout(), color_choice())
}

fn color_choice() -> ColorChoice {
    let args: Vec<String> = std::env::args().collect();
    for (index, arg) in args.iter().enumerate() {
        let value = arg
            .strip_prefix("--color=")
            .or_else(|| arg.strip_prefix("--colour="))
            .or_else(|| {
                arg.strip_prefix("-c")
                    .map(|value| value.strip_prefix('=').unwrap_or(value))
                    .filter(|value| !value.is_empty())
            })
            .or_else(|| {
                if matches!(arg.as_str(), "--color" | "--colour" | "-c") {
                    args.get(index + 1).map(String::as_str)
                } else {
                    None
                }
            });
        match value {
            Some("always") => return ColorChoice::Always,
            Some("never") => return ColorChoice::Never,
            _ => {}
        }
    }
    if std::env::var_os("NO_COLOR").is_some() {
        return ColorChoice::Never;
    }
    ColorChoice::Auto
}

fn statistical_run_requested() -> bool {
    let args: Vec<String> = std::env::args().collect();
    statistical_mode(&args, std::env::var_os("CARGO_CRITERION_PORT").is_some())
}

fn statistical_mode(args: &[String], cargo_criterion: bool) -> bool {
    let has = |flag: &str| {
        args.iter()
            .any(|arg| arg == flag || arg.starts_with(&format!("{flag}=")))
    };
    let bench = has("--bench");
    let test = has("--test");
    let criterion_mode = bench && !test;
    criterion_mode
        && !has("--list")
        && !has("--profile-time")
        && !has("--load-baseline")
        && !args
            .windows(2)
            .any(|pair| pair == ["--output-format", "bencher"])
        && !args.iter().any(|arg| arg == "--output-format=bencher")
        && !cargo_criterion
}

fn discard_baseline_requested() -> bool {
    std::env::args().any(|arg| arg == "--discard-baseline")
}

fn noplot_requested() -> bool {
    std::env::args().any(|arg| matches!(arg.as_str(), "--noplot" | "-n"))
}

fn criterion_roots() -> Vec<PathBuf> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    let criterion_home = std::env::var_os("CRITERION_HOME").map(PathBuf::from);
    let target_dir = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from);
    let metadata_target = if criterion_home.is_none() && target_dir.is_none() {
        cargo_target_directory()
    } else {
        None
    };
    criterion_roots_for(
        &cwd,
        criterion_home.as_deref(),
        target_dir.as_deref(),
        metadata_target.as_deref(),
    )
}

/// Mirrors Criterion's explicit environment overrides and accounts for both
/// default outcomes. Criterion 0.8 runs full `cargo metadata`; when that is
/// unavailable it falls back to `./target/criterion` in the benchmark process's
/// working directory. Our offline, no-dependencies metadata probe can succeed
/// independently. Snapshot both locations and accept only a fresh result.
fn criterion_roots_for(
    cwd: &Path,
    criterion_home: Option<&Path>,
    target_dir: Option<&Path>,
    metadata_target: Option<&Path>,
) -> Vec<PathBuf> {
    let absolute = |path: &Path| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        }
    };
    if let Some(path) = criterion_home {
        return vec![absolute(path)];
    }
    if let Some(path) = target_dir {
        return vec![absolute(path).join("criterion")];
    }
    let mut roots = Vec::new();
    if let Some(path) = metadata_target {
        roots.push(absolute(path).join("criterion"));
    }
    let fallback = cwd.join("target/criterion");
    if !roots.contains(&fallback) {
        roots.push(fallback);
    }
    roots
}

fn report_path_for_display(root: &Path) -> PathBuf {
    report_path_for_manifest(root, Path::new(env!("CARGO_MANIFEST_DIR")))
}

fn report_path_for_manifest(root: &Path, manifest: &Path) -> PathBuf {
    let report = root.join("report/index.html");

    if matches!(manifest.to_str(), Some("/workspace" | "/workspace/benches"))
        && let Ok(host_relative) = report.strip_prefix("/tmp")
    {
        return host_relative.to_path_buf();
    }

    report
        .strip_prefix(manifest)
        .map(Path::to_path_buf)
        .unwrap_or(report)
}

fn cargo_target_directory() -> Option<PathBuf> {
    #[derive(Deserialize)]
    struct Metadata {
        target_directory: PathBuf,
    }

    let cargo = std::env::var_os("CARGO")?;
    let output = Command::new(cargo)
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--offline",
            "--locked",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice::<Metadata>(&output.stdout)
        .ok()
        .map(|metadata| metadata.target_directory)
}

fn cpu_model() -> Option<String> {
    if let Ok(value) = std::env::var("SOLTI_BENCH_CPU")
        && !value.trim().is_empty()
    {
        return Some(value.trim().to_owned());
    }
    if std::env::consts::OS == "macos" {
        for key in ["machdep.cpu.brand_string", "hw.model"] {
            let output = Command::new("sysctl").args(["-n", key]).output().ok()?;
            if output.status.success() {
                let value = String::from_utf8(output.stdout).ok()?;
                if !value.trim().is_empty() {
                    return Some(value.trim().to_owned());
                }
            }
        }
    }
    if std::env::consts::OS == "linux" {
        let cpuinfo = fs::read_to_string("/proc/cpuinfo").ok()?;
        for line in cpuinfo.lines() {
            if let Some((key, value)) = line.split_once(':')
                && matches!(key.trim(), "model name" | "Hardware")
                && !value.trim().is_empty()
            {
                return Some(value.trim().to_owned());
            }
        }
    }
    std::env::var("PROCESSOR_IDENTIFIER").ok()
}

fn git_revision() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let revision = String::from_utf8(output.stdout).ok()?;
    let revision = revision.trim();
    if revision.is_empty() {
        return None;
    }
    let dirty = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=normal"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()
        .is_some_and(|status| status.status.success() && !status.stdout.is_empty());
    Some(format!("{revision}{}", if dirty { "-dirty" } else { "" }))
}

fn enabled_features() -> String {
    let mut features = Vec::new();
    for (enabled, name) in [
        (cfg!(feature = "fixtures"), "fixtures"),
        (cfg!(feature = "subprocess"), "subprocess"),
        (cfg!(feature = "container"), "container"),
        (cfg!(feature = "containerd"), "containerd"),
        (cfg!(feature = "host-policy"), "host-policy"),
        (cfg!(feature = "http"), "http"),
        (cfg!(feature = "discovery"), "discovery"),
        (cfg!(feature = "tls"), "tls"),
        (cfg!(feature = "observability"), "observability"),
    ] {
        if enabled {
            features.push(name);
        }
    }
    if features.is_empty() {
        "core only".to_owned()
    } else {
        features.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAMILY: CaseFamily = CaseFamily::lifecycle(
        "tests/lifecycle",
        "TEST LIFECYCLE",
        "completed task",
        "completed tasks",
        "submit through completion and cleanup",
        "fixture setup",
    );

    fn case(id: &str, family: CaseFamily) -> RecordedCase {
        RecordedCase {
            full_id: id.to_owned(),
            family,
        }
    }

    fn estimate(ns: f64) -> Estimate {
        Estimate {
            confidence_interval: ConfidenceInterval {
                confidence_level: 0.95,
                lower_bound: ns * 0.9,
                upper_bound: ns * 1.1,
            },
            point_estimate: ns,
        }
    }

    fn write_result(root: &Path, ns: f64, units: u64) -> PathBuf {
        let directory = root.join("tests/lifecycle/current_thread/new");
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("benchmark.json"),
            serde_json::to_vec(&serde_json::json!({
                "group_id": FAMILY.group_id,
                "function_id": "current_thread",
                "value_str": null,
                "throughput": { "Elements": units },
                "full_id": "tests/lifecycle/current_thread",
            }))
            .unwrap(),
        )
        .unwrap();
        let path = directory.join("estimates.json");
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "mean": {
                    "point_estimate": ns,
                    "confidence_interval": {
                        "confidence_level": 0.95,
                        "lower_bound": ns * 0.9,
                        "upper_bound": ns * 1.1,
                    },
                },
                "slope": null,
            }))
            .unwrap(),
        )
        .unwrap();
        path
    }

    #[test]
    fn default_workspace_and_package_paths_resolve_the_fresh_package_result() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("sdk");
        let package = workspace.join("benches");
        let target = workspace.join("target");
        let roots = criterion_roots_for(&package, None, None, Some(&target));
        assert_eq!(
            roots,
            vec![target.join("criterion"), package.join("target/criterion")]
        );

        // The report's metadata probe succeeds, but Criterion can fall back to
        // the member cwd. A stale workspace result must not hide that result.
        write_result(&roots[0], 1_000.0, 8);
        let saved = roots
            .iter()
            .flat_map(|root| snapshot_saved_estimates(root))
            .collect();
        write_result(&roots[1], 2_000.0, 8);
        let (observation, actual_root) = load_observation_from_roots(
            &roots,
            case("tests/lifecycle/current_thread", FAMILY),
            &saved,
        )
        .unwrap();
        assert_eq!(actual_root, roots[1]);
        assert_eq!(observation.time.point_estimate, 2_000.0);
    }

    #[test]
    fn default_paths_also_accept_workspace_results_without_reusing_package_results() {
        let temp = tempfile::tempdir().unwrap();
        let package = temp.path().join("sdk/benches");
        let target = temp.path().join("sdk/target");
        let roots = criterion_roots_for(&package, None, None, Some(&target));
        write_result(&roots[1], 1_000.0, 8);
        let saved = roots
            .iter()
            .flat_map(|root| snapshot_saved_estimates(root))
            .collect();
        write_result(&roots[0], 2_000.0, 8);
        let (observation, actual_root) = load_observation_from_roots(
            &roots,
            case("tests/lifecycle/current_thread", FAMILY),
            &saved,
        )
        .unwrap();
        assert_eq!(actual_root, roots[0]);
        assert_eq!(observation.time.point_estimate, 2_000.0);
    }

    #[test]
    fn criterion_environment_overrides_are_exclusive_and_use_process_cwd() {
        let cwd = Path::new("/repo/sdk/benches");
        let metadata = Path::new("/repo/sdk/target");
        assert_eq!(
            criterion_roots_for(
                cwd,
                Some(Path::new("/reports")),
                Some(Path::new("/build")),
                Some(metadata)
            ),
            vec![PathBuf::from("/reports")],
        );
        assert_eq!(
            criterion_roots_for(
                cwd,
                Some(Path::new("reports")),
                Some(Path::new("/build")),
                Some(metadata)
            ),
            vec![cwd.join("reports")],
        );
        assert_eq!(
            criterion_roots_for(cwd, None, Some(Path::new("/build")), Some(metadata)),
            vec![PathBuf::from("/build/criterion")],
        );
        assert_eq!(
            criterion_roots_for(cwd, None, Some(Path::new("build")), Some(metadata)),
            vec![cwd.join("build/criterion")],
        );
        assert_eq!(
            criterion_roots_for(cwd, None, None, None),
            vec![cwd.join("target/criterion")]
        );
        assert_eq!(
            criterion_roots_for(cwd, None, None, Some(&cwd.join("target"))),
            vec![cwd.join("target/criterion")],
        );
    }

    #[test]
    fn two_fresh_default_results_are_rejected_as_ambiguous() {
        let temp = tempfile::tempdir().unwrap();
        let roots = vec![temp.path().join("workspace"), temp.path().join("package")];
        for root in &roots {
            write_result(root, 1_000.0, 8);
        }
        let result = load_observation_from_roots(
            &roots,
            case("tests/lifecycle/current_thread", FAMILY),
            &HashMap::new(),
        );
        assert!(result.err().unwrap().contains("ambiguous"));
    }

    #[test]
    fn container_report_display_handles_the_root_benchmark_package() {
        let root = Path::new("/tmp/solti-sdk-target/criterion");
        let expected = PathBuf::from("solti-sdk-target/criterion/report/index.html");
        assert_eq!(
            report_path_for_manifest(root, Path::new("/workspace")),
            expected
        );
        assert_eq!(
            report_path_for_manifest(root, Path::new("/workspace/benches")),
            expected
        );
        assert_eq!(
            report_path_for_manifest(root, Path::new("/repo/sdk/benches")),
            root.join("report/index.html"),
        );
        assert_eq!(
            report_path_for_manifest(
                Path::new("/repo/sdk/benches/target/criterion"),
                Path::new("/repo/sdk/benches")
            ),
            PathBuf::from("target/criterion/report/index.html"),
        );
    }

    #[test]
    fn lifecycle_labels_do_not_leak_into_named_operations() {
        assert_eq!(scope_description(FAMILY), "COMPLETE MANAGED-TASK LIFECYCLE");
        assert_eq!(
            scope_description(FAMILY.without_lifecycle_interpretation()),
            "COMPLETE LIFECYCLE · COMPLETED TASKS"
        );
        for scope in [Scope::Intake, Scope::Policy, Scope::Query] {
            let operation = CaseFamily {
                scope,
                interpretation: Interpretation::Neutral,
                ..FAMILY
            };
            assert_eq!(
                scope_description(operation),
                "OPERATION RATE, NOT COMPLETED-TASK THROUGHPUT"
            );
        }
    }

    #[test]
    fn summary_does_not_combine_different_lifecycle_rates() {
        assert!(managed_lifecycle_summary(&[]).contains("not measured"));
        assert!(managed_lifecycle_summary(&[1_000.0]).contains("1.000 K"));
        let summary = managed_lifecycle_summary(&[1.0, 1_000_000.0]);
        assert!(summary.contains("2 results"));
        assert!(!summary.contains("completed task lifecycles/s"));
    }

    #[test]
    fn operation_rate_uses_the_complete_batch_time() {
        assert_eq!(rate(8, 2_000_000_000.0), 4.0);
        assert_eq!(format_duration(2_000_000_000.0 / 8.0), "250.000 ms");
        assert_eq!(pluralize_for_count("task", "tasks", 1), "task");
        assert_eq!(pluralize_for_count("task", "tasks", 8), "tasks");
    }

    #[test]
    fn runtime_and_parameter_variants_share_only_their_own_card() {
        let other = CaseFamily {
            group_id: "tests/other",
            ..FAMILY
        };
        let observations = [
            Observation {
                case: case("a", FAMILY),
                function_id: "current_thread".into(),
                value_str: None,
                units: 1,
                time: estimate(10.0),
            },
            Observation {
                case: case("b", FAMILY),
                function_id: "multi_thread".into(),
                value_str: Some("32_tasks".into()),
                units: 32,
                time: estimate(100.0),
            },
            Observation {
                case: case("c", other),
                function_id: "current_thread".into(),
                value_str: None,
                units: 1,
                time: estimate(30.0),
            },
        ];
        let groups = group_observations(&observations);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].observations.len(), 2);
        assert_eq!(groups[1].observations.len(), 1);
        assert_eq!(
            observation_details(&observations[1]),
            "Tokio multi-thread · 4 workers · 32 tasks"
        );
    }

    #[test]
    fn smoke_list_profile_and_loaded_baselines_do_not_claim_fresh_measurements() {
        let mode = |flags: &[&str], external| {
            statistical_mode(
                &flags
                    .iter()
                    .map(|flag| (*flag).to_owned())
                    .collect::<Vec<_>>(),
                external,
            )
        };
        assert!(mode(&["bench", "--bench"], false));
        assert!(!mode(&["bench"], false));
        assert!(!mode(&["bench", "--bench"], true));
        for flag in [
            "--test",
            "--list",
            "--profile-time=1",
            "--load-baseline=old",
            "--output-format=bencher",
        ] {
            assert!(!mode(&["bench", "--bench", flag], false), "{flag}");
        }
        assert!(!mode(
            &["bench", "--bench", "--output-format", "bencher"],
            false
        ));
    }

    #[test]
    fn stale_saved_results_are_not_reported_as_current_measurements() {
        let temp = tempfile::tempdir().unwrap();
        write_result(temp.path(), 1_000.0, 8);
        let saved = snapshot_saved_estimates(temp.path());
        let stale = load_observation(
            temp.path(),
            case("tests/lifecycle/current_thread", FAMILY),
            &saved,
        );
        assert!(stale.err().unwrap().contains("stale"));
        write_result(temp.path(), 2_000.0, 8);
        let fresh = load_observation(
            temp.path(),
            case("tests/lifecycle/current_thread", FAMILY),
            &saved,
        )
        .unwrap();
        assert_eq!(fresh.time.point_estimate, 2_000.0);
        assert_eq!(fresh.units, 8);
    }

    #[test]
    fn missing_and_invalid_results_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let missing = load_observation(
            temp.path(),
            case("tests/lifecycle/current_thread", FAMILY),
            &HashMap::new(),
        );
        assert!(missing.err().unwrap().contains("missing"));
        write_result(temp.path(), 1_000.0, 0);
        let invalid = load_observation(
            temp.path(),
            case("tests/lifecycle/current_thread", FAMILY),
            &HashMap::new(),
        );
        assert!(invalid.err().unwrap().contains("invalid"));
    }

    #[test]
    fn formats_semantic_units_and_wraps_without_losing_words() {
        assert_eq!(format_count(123_456_789.2), "123,456,789");
        assert_eq!(format_rate(1_500.0), "1.500 K");
        assert_eq!(format_duration(1_500.0), "1.500 µs");
        let words = "a measured operation with a known boundary";
        assert_eq!(wrap_words(words, 12).join(" "), words);
    }
}
