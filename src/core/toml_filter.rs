//! Applies TOML-defined filter rules to command output.
///
/// Provides a declarative pipeline of 8 stages that can be configured
/// via TOML files. Lookup priority (first match wins):
///   1. `.rtk/filters.toml`              — project-local, committable with the repo
///   2. `~/.config/rtk/filters.toml`     — user-global, applies to all projects
///   3. Built-in TOML                     — `src/filters/*.toml`, concatenated by build.rs and embedded at compile time
///   4. Passthrough                       — no match, handled by caller
///
/// `nexus init` generates a commented template for both levels (project or global).
///
/// Environment variables:
///   - `RTK_NO_TOML=1`     — bypass TOML engine entirely
///   - `RTK_TOML_DEBUG=1`  — print which filter matched and line counts to stderr
///
/// Pipeline stages (applied in order):
///   1. strip_ansi           — remove ANSI escape codes
///   2. replace              — regex substitutions, line-by-line, chainable
///   3. match_output         — short-circuit: if blob matches a pattern, return message immediately
///   4. strip/keep_lines     — filter lines by regex
///   5. coalesce_directories — collapse Entering/Leaving directory patterns to one line
///   6. dedup_consecutive    — collapse consecutive repeated lines → "[xN]"
///   7. smart_truncate       — keep only structurally important lines up to N
///   8. truncate_lines_at    — truncate each line to N chars
///   9. head/tail_lines      — keep first/last N lines
///   10. max_lines            — absolute line cap
///   11. on_empty            — message if result is empty
use super::constants::{FILTERS_TOML, NEXUS_DATA_DIR};
use lazy_static::lazy_static;
use regex::{Regex, RegexSet};
use serde::Deserialize;
use std::collections::BTreeMap;

// Built-in filters: concatenated from src/filters/*.toml by build.rs at compile time.
const BUILTIN_TOML: &str = include_str!(concat!(env!("OUT_DIR"), "/builtin_filters.toml"));

// ---------------------------------------------------------------------------
// Deserialization types (TOML schema)
// ---------------------------------------------------------------------------

/// A match-output rule: if `pattern` matches anywhere in the full output blob,
/// the filter short-circuits and returns `message` immediately.
/// First matching rule wins; remaining rules are not evaluated.
/// Optional `unless`: if this regex also matches the blob, the rule is skipped
/// (prevents short-circuiting when errors or warnings are present).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MatchOutputRule {
    pattern: String,
    message: String,
    #[serde(default)]
    unless: Option<String>,
}

/// A regex substitution applied line-by-line. Rules are chained sequentially:
/// rule N+1 operates on the output of rule N.
/// Backreferences (`$1`, `$2`, ...) are supported via the `regex` crate.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaceRule {
    pattern: String,
    replacement: String,
}

/// An inline test case attached to a filter in the TOML.
/// Lives in `[[tests.<filter-name>]]` sections, separate from `[filters.*]`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TomlFilterTestDef {
    pub name: String,
    pub input: String,
    pub expected: String,
}

#[derive(Deserialize)]
struct TomlFilterFile {
    schema_version: u32,
    #[serde(default)]
    filters: BTreeMap<String, TomlFilterDef>,
    /// Inline tests keyed by filter name. Kept separate from `filters` so that
    /// `TomlFilterDef` can keep `deny_unknown_fields` without touching test data.
    #[serde(default)]
    tests: BTreeMap<String, Vec<TomlFilterTestDef>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlFilterDef {
    description: Option<String>,
    match_command: String,
    #[serde(default)]
    strip_ansi: bool,
    /// Regex substitutions, applied line-by-line before match_output (stage 2).
    #[serde(default)]
    replace: Vec<ReplaceRule>,
    /// Short-circuit rules: if the full output blob matches, return the message (stage 3).
    #[serde(default)]
    match_output: Vec<MatchOutputRule>,
    #[serde(default)]
    strip_lines_matching: Vec<String>,
    #[serde(default)]
    keep_lines_matching: Vec<String>,
    truncate_lines_at: Option<usize>,
    head_lines: Option<usize>,
    tail_lines: Option<usize>,
    max_lines: Option<usize>,
    /// Smart truncate: keep only structurally important lines (default None = disabled).
    /// When set, discards verbose lines and keeps only error/summary/result lines up to N.
    #[allow(dead_code)]
    smart_truncate: Option<usize>,
    #[serde(default)]
    dedup_consecutive: bool,
    #[serde(default)]
    coalesce_directories: bool,
    /// Normalize timestamps to a short marker to reduce token count.
    /// Replaces common timestamp patterns (ISO dates, time-only, duration,
    /// time-elapsed) with a `{t}` placeholder.
    #[serde(default)]
    normalize_timestamps: bool,
    #[serde(default)]
    error_only: bool,
    on_empty: Option<String>,
    /// When true, stderr is captured and merged with stdout before filtering.
    /// Use for tools like liquibase that emit banners/logs to stderr.
    #[serde(default)]
    filter_stderr: bool,
}

// ---------------------------------------------------------------------------
// Compiled types (post-validation, ready to use)
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct CompiledMatchOutputRule {
    pattern: Regex,
    message: String,
    /// If set and matches the blob, this rule is skipped (prevents swallowing errors).
    unless: Option<Regex>,
}

#[derive(Debug)]
struct CompiledReplaceRule {
    pattern: Regex,
    replacement: String,
}

#[derive(Debug)]
enum LineFilter {
    None,
    Strip(RegexSet),
    Keep(RegexSet),
}

/// A filter that has been parsed and compiled — all regexes are ready.
#[derive(Debug)]
pub struct CompiledFilter {
    pub name: String,
    #[allow(dead_code)]
    pub description: Option<String>,
    match_regex: Regex,
    strip_ansi: bool,
    replace: Vec<CompiledReplaceRule>,
    match_output: Vec<CompiledMatchOutputRule>,
    line_filter: LineFilter,
    truncate_lines_at: Option<usize>,
    head_lines: Option<usize>,
    tail_lines: Option<usize>,
    pub max_lines: Option<usize>,
    pub on_empty: Option<String>,
    pub smart_truncate: Option<usize>,
    pub dedup_consecutive: bool,
    pub coalesce_directories: bool,
    pub normalize_timestamps: bool,
    pub error_only: bool,
    /// When true, the runner should capture stderr and merge it with stdout.
    pub filter_stderr: bool,
}

// ---------------------------------------------------------------------------
// Results for `nexus verify`
// ---------------------------------------------------------------------------

/// Outcome of running a single inline test.
pub struct TestOutcome {
    pub filter_name: String,
    pub test_name: String,
    pub passed: bool,
    pub actual: String,
    pub expected: String,
}

/// Aggregated results from `run_filter_tests`.
pub struct VerifyResults {
    /// Individual test outcomes (all filters, or just the requested one).
    pub outcomes: Vec<TestOutcome>,
    /// Filter names that have no inline tests (used by `--require-all`).
    pub filters_without_tests: Vec<String>,
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

pub struct TomlFilterRegistry {
    pub filters: Vec<CompiledFilter>,
}

impl TomlFilterRegistry {
    /// Load registry from disk + built-in. Emits warnings to stderr on parse
    /// errors but never panics — bad files are silently ignored.
    fn load() -> Self {
        let mut filters = Vec::new();

        // Priority 1: project-local .rtk/filters.toml (trust-gated)
        let project_filter_path = std::path::Path::new(".rtk/filters.toml");
        if project_filter_path.exists() {
            let trust_status = crate::hooks::trust::check_trust(project_filter_path)
                .unwrap_or(crate::hooks::trust::TrustStatus::Untrusted);

            match trust_status {
                crate::hooks::trust::TrustStatus::Trusted
                | crate::hooks::trust::TrustStatus::EnvOverride => {
                    if let Ok(content) = std::fs::read_to_string(project_filter_path) {
                        match Self::parse_and_compile(&content, "project") {
                            Ok(f) => filters.extend(f),
                            Err(e) => eprintln!("[nexus] warning: .rtk/filters.toml: {}", e),
                        }
                    }
                }
                crate::hooks::trust::TrustStatus::Untrusted => {
                    eprintln!("[nexus] WARNING: untrusted project filters (.rtk/filters.toml)");
                    eprintln!("[nexus] Filters NOT applied. Run `nexus trust` to review and enable.");
                }
                crate::hooks::trust::TrustStatus::ContentChanged { .. } => {
                    eprintln!("[nexus] WARNING: .rtk/filters.toml changed since trusted.");
                    eprintln!("[nexus] Filters NOT applied. Run `nexus trust` to re-review.");
                }
            }
        }

        // Priority 2: user-global ~/.config/rtk/filters.toml
        if let Some(config_dir) = dirs::config_dir() {
            let global_path = config_dir.join(NEXUS_DATA_DIR).join(FILTERS_TOML);
            if let Ok(content) = std::fs::read_to_string(&global_path) {
                match Self::parse_and_compile(&content, "user-global") {
                    Ok(f) => filters.extend(f),
                    Err(e) => eprintln!("[nexus] warning: {}: {}", global_path.display(), e),
                }
            }
        }

        // Priority 3: built-in (embedded at compile time)
        let builtin = BUILTIN_TOML;
        match Self::parse_and_compile(builtin, "builtin") {
            Ok(f) => filters.extend(f),
            Err(e) => eprintln!("[nexus] warning: builtin filters: {}", e),
        }

        TomlFilterRegistry { filters }
    }

    fn parse_and_compile(content: &str, source: &str) -> Result<Vec<CompiledFilter>, String> {
        let file: TomlFilterFile = toml::from_str(content)
            .map_err(|e| format!("TOML parse error in {}: {}", source, e))?;

        if file.schema_version != 1 {
            return Err(format!(
                "unsupported schema_version {} in {} (expected 1)",
                file.schema_version, source
            ));
        }

        let mut compiled = Vec::new();
        for (name, def) in file.filters {
            match compile_filter(name.clone(), def) {
                Ok(f) => compiled.push(f),
                Err(e) => eprintln!("[nexus] warning: filter '{}' in {}: {}", name, source, e),
            }
        }
        Ok(compiled)
    }
}

/// Commands already handled by dedicated Rust modules (routed by Clap before TOML).
/// A TOML filter whose match_command matches one of these will never activate —
/// Clap routes the command before `run_fallback()` is reached.
const RUST_HANDLED_COMMANDS: &[&str] = &[
    "ls",
    "tree",
    "read",
    "smart",
    "git",
    "gh",
    "aws",
    "psql",
    "pnpm",
    "err",
    "test",
    "json",
    "deps",
    "env",
    "find",
    "diff",
    "log",
    "docker",
    "kubectl",
    "summary",
    "grep",
    "init",
    "wget",
    "wc",
    "gain",
    "config",
    "vitest",
    "prisma",
    "tsc",
    "next",
    "lint",
    "prettier",
    "format",
    "playwright",
    "cargo",
    "npm",
    "npx",
    "curl",
    "discover",
    "ruff",
    "pytest",
    "mypy",
    "pip",
    "go",
    "golangci-lint",
    "rewrite",
    "proxy",
    "verify",
    "learn",
];

fn compile_filter(name: String, def: TomlFilterDef) -> Result<CompiledFilter, String> {
    // Mutual exclusion: strip and keep cannot both be set
    if !def.strip_lines_matching.is_empty() && !def.keep_lines_matching.is_empty() {
        return Err("strip_lines_matching and keep_lines_matching are mutually exclusive".into());
    }

    let match_regex = Regex::new(&def.match_command)
        .map_err(|e| format!("invalid match_command regex: {}", e))?;

    // Shadow warning: if match_command matches a Rust-handled command, this filter
    // will never activate (Clap routes before run_fallback). Warn the author.
    for cmd in RUST_HANDLED_COMMANDS {
        if match_regex.is_match(cmd) {
            eprintln!(
                "[nexus] warning: filter '{}' match_command matches '{}' which is already \
                 handled by a Rust module — this filter will never activate for that command",
                name, cmd
            );
            break;
        }
    }

    let replace = def
        .replace
        .into_iter()
        .map(|r| {
            let pat = r.pattern.clone();
            Regex::new(&r.pattern)
                .map(|pattern| CompiledReplaceRule {
                    pattern,
                    replacement: r.replacement,
                })
                .map_err(|e| format!("invalid replace pattern '{}': {}", pat, e))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let match_output = def
        .match_output
        .into_iter()
        .map(|r| -> Result<CompiledMatchOutputRule, String> {
            let pat = r.pattern.clone();
            let pattern = Regex::new(&r.pattern)
                .map_err(|e| format!("invalid match_output pattern '{}': {}", pat, e))?;
            let unless = r
                .unless
                .as_deref()
                .map(|u| {
                    Regex::new(u)
                        .map_err(|e| format!("invalid match_output unless pattern '{}': {}", u, e))
                })
                .transpose()?;
            Ok(CompiledMatchOutputRule {
                pattern,
                message: r.message,
                unless,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    let line_filter = if !def.strip_lines_matching.is_empty() {
        let set = RegexSet::new(&def.strip_lines_matching)
            .map_err(|e| format!("invalid strip_lines_matching regex: {}", e))?;
        LineFilter::Strip(set)
    } else if !def.keep_lines_matching.is_empty() {
        let set = RegexSet::new(&def.keep_lines_matching)
            .map_err(|e| format!("invalid keep_lines_matching regex: {}", e))?;
        LineFilter::Keep(set)
    } else {
        LineFilter::None
    };

    Ok(CompiledFilter {
        name,
        description: def.description,
        match_regex,
        strip_ansi: def.strip_ansi,
        replace,
        match_output,
        line_filter,
        truncate_lines_at: def.truncate_lines_at,
        head_lines: def.head_lines,
        tail_lines: def.tail_lines,
        max_lines: def.max_lines,
        on_empty: def.on_empty,
        smart_truncate: def.smart_truncate,
        coalesce_directories: def.coalesce_directories,
        normalize_timestamps: def.normalize_timestamps,
        dedup_consecutive: def.dedup_consecutive,
        error_only: def.error_only,
        filter_stderr: def.filter_stderr,
    })
}

// ---------------------------------------------------------------------------
// Singleton (lazy-loaded, one-time cost)
// ---------------------------------------------------------------------------

lazy_static! {
    static ref REGISTRY: TomlFilterRegistry = TomlFilterRegistry::load();
}

// ---------------------------------------------------------------------------
// Public API — pure functions (testable without global state)
// ---------------------------------------------------------------------------

/// Find the first matching filter in a slice. O(N) on the number of filters.
/// Tests should call this directly with a local filter list.
pub fn find_filter_in<'a>(
    command: &str,
    filters: &'a [CompiledFilter],
) -> Option<&'a CompiledFilter> {
    filters.iter().find(|f| f.match_regex.is_match(command))
}

/// Apply a compiled filter pipeline to raw stdout. Pure String -> String.
///
/// Pipeline stages (in order):
///   1. strip_ansi           — remove ANSI escape codes
///   2. replace              — regex substitutions, line-by-line, chainable
///   3. match_output         — short-circuit if blob matches a pattern
///   4. strip/keep_lines     — filter lines by regex
///   5. truncate_lines_at    — truncate each line to N chars
///   6. head/tail_lines      — keep first/last N lines
///   7. max_lines            — absolute line cap
///   8. on_empty             — message if result is empty
pub fn apply_filter(filter: &CompiledFilter, stdout: &str) -> String {
    let mut lines: Vec<String> = stdout.lines().map(String::from).collect();

    // 1. strip_ansi
    if filter.strip_ansi {
        lines = lines
            .into_iter()
            .map(|l| crate::core::utils::strip_ansi(&l))
            .collect();
    }

    // 2. replace — line-by-line, rules chained sequentially
    if !filter.replace.is_empty() {
        lines = lines
            .into_iter()
            .map(|mut line| {
                for rule in &filter.replace {
                    line = rule
                        .pattern
                        .replace_all(&line, rule.replacement.as_str())
                        .into_owned();
                }
                line
            })
            .collect();
    }

    // 3. match_output — short-circuit on full blob match (first rule wins)
    //    If `unless` is set and also matches the blob, the rule is skipped.
    if !filter.match_output.is_empty() {
        let blob = lines.join("\n");
        for rule in &filter.match_output {
            if rule.pattern.is_match(&blob) {
                if let Some(ref unless_re) = rule.unless {
                    if unless_re.is_match(&blob) {
                        continue; // errors/warnings present — skip this rule
                    }
                }
                return rule.message.clone();
            }
        }
    }

    // 4. strip OR keep (mutually exclusive)
    match &filter.line_filter {
        LineFilter::Strip(set) => lines.retain(|l| !set.is_match(l)),
        LineFilter::Keep(set) => lines.retain(|l| set.is_match(l)),
        LineFilter::None => {}
    }

    // 5. coalesce_directories — collapse Entering/Leaving directory patterns
    if filter.coalesce_directories {
        coalesce_directory_lines(&mut lines);
    }

    // 6. dedup_consecutive — collapse repeated consecutive lines
    if filter.dedup_consecutive {
        lines = dedup_lines(&lines);
    }

    // 7. smart_truncate — keep only structurally important lines up to N
    if let Some(max) = filter.smart_truncate {
        if lines.len() > max {
            lines = smart_truncate_output(&lines, max);
        }
    }

    // 8. normalize_timestamps — replace timestamps with {t} placeholder
    if filter.normalize_timestamps {
        for line in &mut lines {
            normalize_timestamps_in_line(line);
        }
    }

    // 9. truncate_lines_at — uses utils::truncate (unicode-safe)
    if let Some(max_chars) = filter.truncate_lines_at {
        lines = lines
            .into_iter()
            .map(|l| crate::core::utils::truncate(&l, max_chars))
            .collect();
    }

    // 10. head + tail
    let total = lines.len();
    if let (Some(head), Some(tail)) = (filter.head_lines, filter.tail_lines) {
        if total > head + tail {
            let mut result = lines[..head].to_vec();
            result.push(format!("... ({} lines omitted)", total - head - tail));
            result.extend_from_slice(&lines[total - tail..]);
            lines = result;
        }
    } else if let Some(head) = filter.head_lines {
        if total > head {
            lines.truncate(head);
            lines.push(format!("... ({} lines omitted)", total - head));
        }
    } else if let Some(tail) = filter.tail_lines {
        if total > tail {
            let omitted = total - tail;
            lines = lines[omitted..].to_vec();
            lines.insert(0, format!("... ({} lines omitted)", omitted));
        }
    }

    // 11. max_lines — absolute cap applied after head/tail (includes omit messages)
    if let Some(max) = filter.max_lines {
        if lines.len() > max {
            let truncated = lines.len() - max;
            lines.truncate(max);
            lines.push(format!("... ({} lines truncated)", truncated));
        }
    }

    // 12. on_empty
    let result = lines.join("\n");
    if result.trim().is_empty() {
        if let Some(ref msg) = filter.on_empty {
            return msg.clone();
        }
    }

    result
}

/// Collapse consecutive repeated lines into `"content [xN]"`.
///
/// # Examples
///
/// ```
/// let input = vec!["a".to_string(), "a".to_string(), "b".to_string(), "a".to_string(), "a".to_string(), "a".to_string()];
/// let out = dedup_lines(&input);
/// assert_eq!(out, vec!["a [x2]", "b", "a [x3]"]);
/// ```
pub fn dedup_lines(lines: &[String]) -> Vec<String> {
    if lines.is_empty() {
        return Vec::new();
    }
    let mut result: Vec<String> = Vec::with_capacity(lines.len());
    let mut count = 1u32;
    let mut prev = &lines[0];

    for line in &lines[1..] {
        if line == prev {
            count += 1;
        } else {
            if count > 1 {
                result.push(format!("{} [x{}]", prev, count));
            } else {
                result.push(prev.clone());
            }
            prev = line;
            count = 1;
        }
    }
    // flush last run
    if count > 1 {
        result.push(format!("{} [x{}]", prev, count));
    } else {
        result.push(prev.clone());
    }

    result
}

/// Collapse Entering/Leaving directory pairs into compact lines.
///
/// Scans for patterns like `Entering directory '/path'` and `Leaving directory '/path'`,
/// collapses matched pairs into a single `=> /path` line, and removes matching leaving lines.
/// Unmatched entering/leaving lines are kept as-is.
///
/// # Examples
///
/// ```text
/// make[1]: Entering directory '/home/user/src'
/// gcc -O2 foo.c
/// make[1]: Leaving directory '/home/user/src'
/// ```
/// becomes:
/// ```text
/// => /home/user/src
/// gcc -O2 foo.c
/// ```
pub fn coalesce_directory_lines(lines: &mut Vec<String>) {
    lazy_static! {
        static ref ENTER_RE: Regex =
            Regex::new(r#"(?i)(?:.*?\s)?Entering\s+directory\s+['"](.+)['"]\s*$"#).unwrap();
        static ref LEAVE_RE: Regex =
            Regex::new(r#"(?i)(?:.*?\s)?Leaving\s+directory\s+['"](.+)['"]\s*$"#).unwrap();
    }

    let mut i = 0;
    while i < lines.len() {
        let trimmed_i = lines[i].trim();
        if let Some(caps) = ENTER_RE.captures(trimmed_i) {
            let enter_dir = caps.get(1).unwrap().as_str().to_string();
            // Find matching leaving line
            let mut j = i + 1;
            let mut depth = 1u32;
            while j < lines.len() {
                let trimmed_j = lines[j].trim();
                if let Some(_lcaps) = ENTER_RE.captures(trimmed_j) {
                    depth += 1;
                } else if let Some(lcaps) = LEAVE_RE.captures(trimmed_j) {
                    if depth == 1 && lcaps.get(1).unwrap().as_str() == enter_dir {
                        // Found match — collapse entering, remove leaving
                        lines[i] = format!("=> {}", enter_dir);
                        lines.remove(j);
                        break;
                    }
                    depth -= 1;
                }
                j += 1;
            }
            // If no match found, keep original line — don't modify
        }
        i += 1;
    }
}

/// Normalize common timestamp patterns in a line to `{t}` placeholder.
///
/// Handles ISO-format datetimes, time-only timestamps, duration strings,
/// and time-elapsed patterns. Runs per-line in the pipeline stage.
///
/// # Examples
///
/// ```text
/// "2024-01-15 10:30:00,123 INFO"  →  "{t} INFO"
/// "[15:30:45] Starting build"     →  "{t} Starting build"
/// "Duration: 3.2s"                →  "Duration: {t}"
/// ```
pub fn normalize_timestamps_in_line(line: &mut String) {
    lazy_static! {
        // ISO datetime: 2024-01-15T10:30:00Z or 2024-01-15 10:30:00,123
        static ref ISO_DT: Regex =
            Regex::new(r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:[.,]\d+)?(?:Z|[+-]\d{2}:\d{2})?")
            .unwrap();
        // Bracketed time: [15:30:45]
        static ref BRACKET_TIME: Regex =
            Regex::new(r"\[\d{2}:\d{2}:\d{2}\]").unwrap();
        // Duration value: "3.2s" or "123ms" after label
        static ref DURATION: Regex =
            Regex::new(r"(?i)(\bDuration:\s*)\d+\.?\d*\s*(?:s|ms|µs|us|ns)\b").unwrap();
        // Time elapsed: "Time Elapsed 00:00:02.34"
        static ref TIME_ELAPSED: Regex =
            Regex::new(r"(?i)(Time\s+Elapsed\s+)\d{2}:\d{2}:\d{2}\.\d{2}").unwrap();
        // Timestamp prefix: "15:30:45 INFO" at start of line
        static ref TIME_PREFIX: Regex =
            Regex::new(r"^\d{2}:\d{2}:\d{2}(?:,\d{3})?\s+").unwrap();
    }

    if ISO_DT.is_match(line)
        || BRACKET_TIME.is_match(line)
        || DURATION.is_match(line)
        || TIME_ELAPSED.is_match(line)
        || TIME_PREFIX.is_match(line)
    {
        let replaced = ISO_DT.replace_all(line, "{t}");
        let replaced = BRACKET_TIME.replace_all(&replaced, "{t}");
        let replaced = DURATION.replace_all(&replaced, "${1}{t}");
        let replaced = TIME_ELAPSED.replace_all(&replaced, "${1}{t}");
        let replaced = TIME_PREFIX.replace_all(&replaced, "{t} ");
        *line = replaced.to_string();
    }
}

/// Score each line by structural importance, keep the top N, and append
/// a `[N more lines]` footer. Used by the `smart_truncate` pipeline stage.
///
/// Scoring rules:
///   - Error/failure lines         → +100
///   - Summary/result lines        → +80
///   - File:line references        → +60
///   - Warning lines               → +50
///   - Package/file change lines   → +30  (lines starting with `+`/`-`)
///   - Last 2 lines (if digits)    → +30  (positional boost for summaries)
///   - Non-empty lines             → +10  (baseline)
///   - First line                  → +5   (slight boost for headers)
///   - Empty/whitespace lines      → −50
pub fn smart_truncate_output(lines: &[String], max_lines: usize) -> Vec<String> {
    struct ScoredLine {
        line: String,
        score: i32,
        index: usize,
    }

    let n = lines.len();
    let mut scored: Vec<ScoredLine> = lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let trimmed = l.trim();
            let mut score: i32 = 0;

            // Error/failure: highest priority
            if trimmed.contains("Error")
                || trimmed.contains("error")
                || trimmed.contains("FAIL")
                || trimmed.contains("fail")
                || trimmed.contains("FAILED")
                || trimmed.ends_with("Failed")
            {
                score += 100;
            }

            // Warning: medium-high
            if trimmed.contains("warning") || trimmed.contains("Warning") || trimmed.contains("WARN")
            {
                score += 50;
            }

            // Summary/result lines
            if trimmed.contains("passed")
                || trimmed.contains("failed")
                || trimmed.contains("skipped")
                || trimmed.contains("complete")
                || trimmed.contains("Summary")
                || trimmed.contains("summary")
                || trimmed.starts_with("Plan:")
                || trimmed.starts_with("Result:")
                || trimmed.contains(" installed")
                || trimmed.contains(" updated")
            {
                score += 80;
            }

            // File:line references (compiler output)
            if trimmed.contains(".rs:")
                || trimmed.contains(".ts:")
                || trimmed.contains(".js:")
                || trimmed.contains(".py:")
                || trimmed.contains(".go:")
                || trimmed.contains(".c:")
            {
                score += 60;
            }

            // Package lines (starting with +/- like pip/npm output)
            if trimmed.starts_with("+ ") || trimmed.starts_with("- ") {
                score += 30;
            }

            // Baseline for non-empty lines
            if !trimmed.is_empty() {
                score += 10;
            }

            // Empty/whitespace: lowest priority
            if trimmed.is_empty() {
                score -= 50;
            }

            // Last 2 lines with digits get a positional boost (often summary)
            if i >= n.saturating_sub(2)
                && (trimmed.chars().any(|c| c.is_ascii_digit())
                    || trimmed.contains("complete")
                    || trimmed.contains("Summary"))
            {
                score += 30;
            }

            // Slight first-line boost for summary headers
            if i == 0 {
                score += 5;
            }

            // Lines with numbers (often stats/summary)
            if trimmed.chars().any(|c| c.is_ascii_digit()) {
                score += 5;
            }

            ScoredLine {
                line: l.clone(),
                score,
                index: i,
            }
        })
        .collect();

    // Sort by score descending, then by original position for ties
    scored.sort_by(|a, b| b.score.cmp(&a.score).then(a.index.cmp(&b.index)));

    // Reserve one slot for the "[N more lines]" footer
    let keep = max_lines.saturating_sub(1);
    scored.truncate(keep);

    // Restore original positional order
    scored.sort_by(|a, b| a.index.cmp(&b.index));

    let mut result: Vec<String> = scored.into_iter().map(|s| s.line).collect();
    let omitted = n - result.len();
    if omitted > 0 {
        result.push(format!("[{} more lines]", omitted));
    }

    result
}

// ---------------------------------------------------------------------------
// rtk verify — inline test execution
// ---------------------------------------------------------------------------

/// Run inline tests from loaded TOML files (builtin + project-local).
///
/// - `filter_name_opt`: if `Some`, only run tests for that filter name.
/// - Returns `VerifyResults` with all outcomes and filters that have no tests.
pub fn run_filter_tests(filter_name_opt: Option<&str>) -> VerifyResults {
    let mut outcomes = Vec::new();
    let mut all_filter_names: Vec<String> = Vec::new();
    let mut tested_filter_names: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    let builtin = BUILTIN_TOML;
    collect_test_outcomes(
        builtin,
        filter_name_opt,
        &mut outcomes,
        &mut all_filter_names,
        &mut tested_filter_names,
    );

    // Trust-gated: only verify project-local filters if trusted (SA-2025-RTK-002)
    let project_path = std::path::Path::new(".rtk/filters.toml");
    if project_path.exists() {
        let trust_status = crate::hooks::trust::check_trust(project_path)
            .unwrap_or(crate::hooks::trust::TrustStatus::Untrusted);
        match trust_status {
            crate::hooks::trust::TrustStatus::Trusted
            | crate::hooks::trust::TrustStatus::EnvOverride => {
                if let Ok(content) = std::fs::read_to_string(project_path) {
                    collect_test_outcomes(
                        &content,
                        filter_name_opt,
                        &mut outcomes,
                        &mut all_filter_names,
                        &mut tested_filter_names,
                    );
                }
            }
            _ => {
                eprintln!("[nexus] WARNING: untrusted project filters skipped in verify");
            }
        }
    }

    let filters_without_tests = all_filter_names
        .into_iter()
        .filter(|name| {
            // When a specific filter is requested, only report that one as missing tests
            filter_name_opt.is_none_or(|f| name == f)
        })
        .filter(|name| !tested_filter_names.contains(name))
        .collect();

    VerifyResults {
        outcomes,
        filters_without_tests,
    }
}

fn collect_test_outcomes(
    content: &str,
    filter_name_opt: Option<&str>,
    outcomes: &mut Vec<TestOutcome>,
    all_filter_names: &mut Vec<String>,
    tested_filter_names: &mut std::collections::HashSet<String>,
) {
    let file: TomlFilterFile = match toml::from_str(content) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[nexus] warning: TOML parse error during verify: {}", e);
            return;
        }
    };

    // Compile all filters and track their names
    let mut compiled_filters: BTreeMap<String, CompiledFilter> = BTreeMap::new();
    for (name, def) in file.filters {
        all_filter_names.push(name.clone());
        match compile_filter(name.clone(), def) {
            Ok(f) => {
                compiled_filters.insert(name, f);
            }
            Err(e) => eprintln!("[nexus] warning: filter '{}' compilation error: {}", name, e),
        }
    }

    // Run tests
    for (filter_name, tests) in file.tests {
        if let Some(name) = filter_name_opt {
            if filter_name != name {
                continue;
            }
        }

        tested_filter_names.insert(filter_name.clone());

        let compiled = match compiled_filters.get(&filter_name) {
            Some(f) => f,
            None => {
                eprintln!(
                    "[nexus] warning: [[tests.{}]] references unknown filter",
                    filter_name
                );
                continue;
            }
        };

        for test in tests {
            let actual = apply_filter(compiled, &test.input);
            // Trim trailing newlines: TOML multiline strings end with a newline
            let actual_cmp = actual.trim_end_matches('\n').to_string();
            let expected_cmp = test.expected.trim_end_matches('\n').to_string();
            outcomes.push(TestOutcome {
                filter_name: filter_name.clone(),
                test_name: test.name,
                passed: actual_cmp == expected_cmp,
                actual: actual_cmp,
                expected: expected_cmp,
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Convenience wrapper (uses singleton — for run_fallback)
// ---------------------------------------------------------------------------

/// Find a matching filter from the global registry. Initialises the registry
/// lazily on first call. Returns `None` if no filter matches.
pub fn find_matching_filter(command: &str) -> Option<&'static CompiledFilter> {
    if std::env::var("RTK_TOML_DEBUG").is_ok() {
        eprintln!(
            "[rtk:toml] looking up filter for: {:?} ({} filters loaded)",
            command,
            REGISTRY.filters.len()
        );
    }
    let result = find_filter_in(command, &REGISTRY.filters);
    if std::env::var("RTK_TOML_DEBUG").is_ok() {
        match result {
            Some(f) => eprintln!("[rtk:toml] matched filter: '{}'", f.name),
            None => eprintln!("[rtk:toml] no filter matched — passthrough"),
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: build a CompiledFilter from inline TOML for tests.
    // Never touches the lazy_static registry.
    fn make_filters(toml: &str) -> Vec<CompiledFilter> {
        TomlFilterRegistry::parse_and_compile(toml, "test").expect("test TOML should be valid")
    }

    fn first_filter(toml: &str) -> CompiledFilter {
        make_filters(toml)
            .into_iter()
            .next()
            .expect("expected at least one filter")
    }

    // --- Pipeline primitives (existing) ---

    #[test]
    fn test_strip_ansi_removes_codes() {
        let f = first_filter(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
strip_ansi = true
"#,
        );
        let out = apply_filter(&f, "\x1b[31mError\x1b[0m\nnormal");
        assert_eq!(out, "Error\nnormal");
    }

    #[test]
    fn test_strip_lines_matching_basic() {
        let f = first_filter(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
strip_lines_matching = ["^noise", "^verbose"]
"#,
        );
        let input = "noise line\nkeep this\nverbose stuff\nalso keep";
        let out = apply_filter(&f, input);
        assert_eq!(out, "keep this\nalso keep");
    }

    #[test]
    fn test_keep_lines_matching_basic() {
        let f = first_filter(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
keep_lines_matching = ["^PASS", "^FAIL"]
"#,
        );
        let input = "PASS test_a\nsome noise\nFAIL test_b\nmore noise";
        let out = apply_filter(&f, input);
        assert_eq!(out, "PASS test_a\nFAIL test_b");
    }

    #[test]
    fn test_truncate_lines_at_unicode_safe() {
        let f = first_filter(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
truncate_lines_at = 5
"#,
        );
        // utils::truncate(s, 5) takes 2 chars + "..." when len > 5
        // "hello" = 5 chars exactly, stays unchanged
        // "日本語xyz" = 6 chars, truncated to "日本..." (take 2 + "...")
        let out = apply_filter(&f, "hello\n日本語xyz");
        assert_eq!(out, "hello\n日本...");
    }

    #[test]
    fn test_head_lines() {
        let f = first_filter(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
head_lines = 2
"#,
        );
        let input = "a\nb\nc\nd\ne";
        let out = apply_filter(&f, input);
        assert!(out.starts_with("a\nb\n"));
        assert!(out.contains("3 lines omitted"));
    }

    #[test]
    fn test_tail_lines() {
        let f = first_filter(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
tail_lines = 2
"#,
        );
        let input = "a\nb\nc\nd\ne";
        let out = apply_filter(&f, input);
        assert!(out.contains("3 lines omitted"));
        assert!(out.ends_with("d\ne"));
    }

    #[test]
    fn test_head_and_tail_combined() {
        let f = first_filter(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
head_lines = 2
tail_lines = 2
"#,
        );
        let input = "a\nb\nc\nd\ne\nf";
        let out = apply_filter(&f, input);
        assert!(out.starts_with("a\nb\n"));
        assert!(out.contains("2 lines omitted"));
        assert!(out.ends_with("e\nf"));
    }

    #[test]
    fn test_max_lines_counts_omit_message() {
        // max_lines applied AFTER head — the "omitted" message counts as a line
        let f = first_filter(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
max_lines = 3
"#,
        );
        let input = "a\nb\nc\nd\ne";
        let out = apply_filter(&f, input);
        let line_count = out.lines().count();
        // 3 content lines + 1 truncated message = 4 lines output
        assert_eq!(line_count, 4);
        assert!(out.contains("lines truncated"));
    }

    #[test]
    fn test_on_empty_when_all_filtered() {
        let f = first_filter(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
strip_lines_matching = [".*"]
on_empty = "nothing left"
"#,
        );
        let out = apply_filter(&f, "line1\nline2");
        assert_eq!(out, "nothing left");
    }

    #[test]
    fn test_on_empty_not_triggered_when_output_remains() {
        let f = first_filter(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
keep_lines_matching = ["keep"]
on_empty = "nothing left"
"#,
        );
        let out = apply_filter(&f, "keep this\nnoise");
        assert_eq!(out, "keep this");
    }

    #[test]
    fn test_full_pipeline_order() {
        // Verify all 8 stages fire in order on a single input
        let f = first_filter(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
strip_ansi = true
strip_lines_matching = ["^noise"]
truncate_lines_at = 10
head_lines = 3
max_lines = 4
on_empty = "empty"
"#,
        );
        let input =
            "\x1b[31mred line\x1b[0m\nnoise skip\nkeep one\nkeep two\nkeep three\nkeep four";
        let out = apply_filter(&f, input);
        // After strip_ansi: "red line", strip noise: removed, head 3 from remaining 4 lines
        assert!(out.contains("red line"));
        assert!(!out.contains("noise skip"));
        assert!(out.contains("lines omitted") || out.contains("lines truncated"));
    }

    // --- Validation ---

    #[test]
    fn test_mutual_exclusion_strip_keep_errors() {
        let result = make_filters(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
strip_lines_matching = ["a"]
keep_lines_matching = ["b"]
"#,
        );
        // The filter should be skipped (warning emitted), resulting in empty list
        assert!(result.is_empty());
    }

    #[test]
    fn test_invalid_regex_returns_err() {
        let result = make_filters(
            r#"
schema_version = 1
[filters.f]
match_command = "["
"#,
        );
        assert!(result.is_empty());
    }

    #[test]
    fn test_schema_version_mismatch_errors() {
        let result = TomlFilterRegistry::parse_and_compile(
            r#"schema_version = 99
[filters.f]
match_command = "^cmd"
"#,
            "test",
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_unknown_field_typo_errors() {
        // deny_unknown_fields should catch this
        let result = TomlFilterRegistry::parse_and_compile(
            r#"schema_version = 1
[filters.f]
match_command = "^cmd"
strip_ansi_typo = true
"#,
            "test",
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_empty_filter_passthrough() {
        let f = first_filter(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
"#,
        );
        let input = "line1\nline2\nline3";
        let out = apply_filter(&f, input);
        assert_eq!(out, input);
    }

    // --- Registry / find ---

    #[test]
    fn test_builtin_filters_compile() {
        // Compile-time safety: panics if any src/filters/*.toml is broken
        let builtin = BUILTIN_TOML;
        let result = TomlFilterRegistry::parse_and_compile(builtin, "builtin");
        assert!(
            result.is_ok(),
            "builtin filters failed to compile: {:?}",
            result
        );
        assert!(!result.unwrap().is_empty());
    }

    #[test]
    fn test_find_filter_matches_terraform() {
        let filters = make_filters(
            r#"
schema_version = 1
[filters.terraform-plan]
match_command = "^terraform\\s+plan"
strip_ansi = true
"#,
        );
        let found = find_filter_in("terraform plan -out=tfplan", &filters);
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "terraform-plan");
    }

    #[test]
    fn test_find_filter_no_match_returns_none() {
        let filters = make_filters(
            r#"
schema_version = 1
[filters.f]
match_command = "^terraform"
"#,
        );
        let found = find_filter_in("kubectl get pods", &filters);
        assert!(found.is_none());
    }

    #[test]
    fn test_project_filters_priority_over_builtin() {
        // Project filter has same name but different max_lines — project wins
        let project = make_filters(
            r#"
schema_version = 1
[filters.make]
match_command = "^make\\b"
max_lines = 999
"#,
        );
        let builtin = make_filters(BUILTIN_TOML);

        // Simulate the registry: project first
        let mut all = project;
        all.extend(builtin);

        let found = find_filter_in("make all", &all).expect("should match");
        assert_eq!(found.name, "make");
        // The first (project) match has max_lines=999
        assert_eq!(found.max_lines, Some(999));
    }

    // --- Token savings ---

    #[test]
    fn test_terraform_savings_above_60pct() {
        let filters = make_filters(BUILTIN_TOML);
        let filter = find_filter_in("terraform plan", &filters).expect("terraform-plan built-in");

        // Inline fixture: realistic terraform plan with many Refreshing state lines (noise).
        // Real infra refreshes 30+ resources; the plan section is small.
        // All Refreshing/lock/blank/unchanged lines are stripped -> >60% savings.
        let input = concat!(
            "Acquiring state lock. This may take a few moments...\n",
            "Refreshing state... [id=vpc-0a1b2c3d]\n",
            "Refreshing state... [id=subnet-11111111]\n",
            "Refreshing state... [id=subnet-22222222]\n",
            "Refreshing state... [id=subnet-33333333]\n",
            "Refreshing state... [id=subnet-44444444]\n",
            "Refreshing state... [id=igw-aabbccdd]\n",
            "Refreshing state... [id=rtb-aabbccdd]\n",
            "Refreshing state... [id=rtb-11223344]\n",
            "Refreshing state... [id=sg-00112233]\n",
            "Refreshing state... [id=sg-44556677]\n",
            "Refreshing state... [id=sg-88990011]\n",
            "Refreshing state... [id=nacl-00aabbcc]\n",
            "Refreshing state... [id=acm-arn:us-east-1:cert/abc]\n",
            "Refreshing state... [id=Z1234567890ABC]\n",
            "Refreshing state... [id=alb-arn:my-alb]\n",
            "Refreshing state... [id=tg-arn:my-tg]\n",
            "Refreshing state... [id=db-ABCDEFGHIJKLMNO]\n",
            "Refreshing state... [id=rds-cluster:my-aurora]\n",
            "Refreshing state... [id=elasticache:my-cluster]\n",
            "Refreshing state... [id=lambda:my-api-function]\n",
            "Refreshing state... [id=lambda:my-worker]\n",
            "Refreshing state... [id=iam-role:my-lambda-role]\n",
            "Refreshing state... [id=iam-role:my-ecs-role]\n",
            "Refreshing state... [id=s3:::my-app-assets]\n",
            "Refreshing state... [id=s3:::my-app-logs]\n",
            "Refreshing state... [id=cloudfront:ABCDEFGHIJK]\n",
            "Refreshing state... [id=ssm:/my/app/db-url]\n",
            "Refreshing state... [id=ssm:/my/app/api-key]\n",
            "Refreshing state... [id=secretsmanager:my-secret]\n",
            "Releasing state lock. This may take a few moments...\n",
            "\n",
            "Terraform will perform the following actions:\n",
            "\n",
            "  # aws_instance.web will be created\n",
            "  + resource \"aws_instance\" \"web\" {\n",
            "      + ami           = \"ami-0c55b159cbfafe1f0\"\n",
            "      + instance_type = \"t3.micro\"\n",
            "    }\n",
            "\n",
            "Plan: 1 to add, 0 to change, 0 to destroy.\n",
        );
        let out = apply_filter(filter, input);
        let input_words = input.split_whitespace().count();
        let out_words = out.split_whitespace().count();
        let savings = 100.0 - (out_words as f64 / input_words as f64 * 100.0);
        assert!(
            savings >= 60.0,
            "terraform-plan filter: expected >=60% savings, got {:.1}% (in={} out={})",
            savings,
            input_words,
            out_words
        );
    }

    #[test]
    fn test_make_savings_above_60pct() {
        let filters = make_filters(BUILTIN_TOML);
        let filter = find_filter_in("make all", &filters).expect("make built-in");

        let input = r#"make[1]: Entering directory '/home/user/project'
make[2]: Entering directory '/home/user/project/src'
gcc -O2 -Wall -c foo.c -o foo.o

make[2]: Nothing to be done for 'install'.
make[3]: Entering directory '/home/user/project/src/lib'
ar rcs libfoo.a foo.o bar.o baz.o
make[3]: Leaving directory '/home/user/project/src/lib'
make[2]: Leaving directory '/home/user/project/src'

make[1]: Leaving directory '/home/user/project'
gcc -O2 -Wall -c bar.c -o bar.o

gcc -O2 -Wall -c baz.c -o baz.o

make[1]: Entering directory '/home/user/project/test'
make[2]: Entering directory '/home/user/project/test/unit'
./run_tests --verbose
make[2]: Nothing to be done for 'check'.
make[2]: Leaving directory '/home/user/project/test/unit'
make[1]: Leaving directory '/home/user/project/test'

ld -o myapp foo.o bar.o baz.o -lfoo

make[1]: Entering directory '/home/user/project/docs'
doxygen Doxyfile
make[1]: Leaving directory '/home/user/project/docs'
"#;
        let out = apply_filter(filter, input);
        let input_words = input.split_whitespace().count();
        let out_words = out.split_whitespace().count();
        let savings = 100.0 - (out_words as f64 / input_words as f64 * 100.0);
        assert!(
            savings >= 60.0,
            "make filter: expected >=60% savings, got {:.1}% (in={} out={})",
            savings,
            input_words,
            out_words
        );
    }

    // --- Edge cases ---

    #[test]
    fn test_empty_input() {
        let f = first_filter(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
strip_lines_matching = [".*"]
"#,
        );
        let out = apply_filter(&f, "");
        assert_eq!(out, "");
    }

    #[test]
    fn test_unicode_preserved() {
        let f = first_filter(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
strip_lines_matching = ["^noise"]
"#,
        );
        let out = apply_filter(&f, "日本語テスト\nnoise\n中文内容");
        assert_eq!(out, "日本語テスト\n中文内容");
    }

    // --- match_output tests (PR1) ---

    #[test]
    fn test_match_output_basic_short_circuit() {
        let f = first_filter(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
match_output = [
  { pattern = "Switched to branch", message = "ok" },
]
"#,
        );
        let out = apply_filter(&f, "Switched to branch 'main'");
        assert_eq!(out, "ok");
    }

    #[test]
    fn test_match_output_second_rule_matches() {
        let f = first_filter(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
match_output = [
  { pattern = "Switched to branch", message = "switched" },
  { pattern = "Already on", message = "already" },
]
"#,
        );
        let out = apply_filter(&f, "Already on 'main'");
        assert_eq!(out, "already");
    }

    #[test]
    fn test_match_output_no_match_pipeline_continues() {
        let f = first_filter(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
match_output = [
  { pattern = "Switched to branch", message = "ok" },
]
strip_lines_matching = ["^noise"]
"#,
        );
        let out = apply_filter(&f, "noise\nkeep this");
        // No match_output match, pipeline continues and strips noise
        assert_eq!(out, "keep this");
    }

    #[test]
    fn test_match_output_strip_ansi_before_match() {
        let f = first_filter(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
strip_ansi = true
match_output = [
  { pattern = "Switched to branch", message = "ok" },
]
"#,
        );
        // ANSI stripped before match_output check (stage 1 before stage 3)
        let out = apply_filter(&f, "\x1b[32mSwitched to branch\x1b[0m 'main'");
        assert_eq!(out, "ok");
    }

    #[test]
    fn test_match_output_no_match_then_on_empty() {
        let f = first_filter(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
match_output = [
  { pattern = "Switched", message = "ok" },
]
strip_lines_matching = [".*"]
on_empty = "nothing"
"#,
        );
        // No match_output match; pipeline strips all lines; on_empty fires
        let out = apply_filter(&f, "foo bar baz");
        assert_eq!(out, "nothing");
    }

    #[test]
    fn test_match_output_invalid_regex_rejected() {
        let result = make_filters(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
match_output = [
  { pattern = "[invalid", message = "ok" },
]
"#,
        );
        assert!(result.is_empty());
    }

    // --- match_output unless tests (PR3) ---

    #[test]
    fn test_match_output_unless_blocks_short_circuit_when_errors_present() {
        // "total size is" matches, but "error" also matches — unless fires, rule is skipped.
        let f = first_filter(
            r#"
schema_version = 1
[filters.f]
match_command = "^rsync"
match_output = [
  { pattern = "total size is", message = "ok (synced)", unless = "error|failed" },
]
"#,
        );
        let input = "rsync: [sender] error\ntotal size is 1000  speedup is 3.33\n";
        let out = apply_filter(&f, input);
        // Should NOT return "ok (synced)" because "error" matches the unless pattern
        assert_ne!(
            out.trim(),
            "ok (synced)",
            "unless should have blocked short-circuit when errors are present"
        );
        // The raw lines should pass through (no further strip rules in this filter)
        assert!(out.contains("error"));
    }

    #[test]
    fn test_match_output_unless_allows_short_circuit_when_no_errors() {
        // "total size is" matches and "error" does NOT appear — unless does not fire, rule wins.
        let f = first_filter(
            r#"
schema_version = 1
[filters.f]
match_command = "^rsync"
match_output = [
  { pattern = "total size is", message = "ok (synced)", unless = "error|failed" },
]
"#,
        );
        let input = "file.txt\ntotal size is 98765  speedup is 77.31\n";
        let out = apply_filter(&f, input);
        assert_eq!(out.trim(), "ok (synced)");
    }

    #[test]
    fn test_match_output_unless_falls_through_to_next_rule() {
        // First rule blocked by unless; second rule (no unless) should match.
        let f = first_filter(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
match_output = [
  { pattern = "success", message = "ok", unless = "error" },
  { pattern = "success", message = "ok with warnings" },
]
"#,
        );
        let input = "success\nerror: something went wrong\n";
        let out = apply_filter(&f, input);
        // First rule skipped (unless matched), second rule (no unless) fires
        assert_eq!(out.trim(), "ok with warnings");
    }

    #[test]
    fn test_match_output_unless_no_field_behaves_like_before() {
        // When unless is absent, behaviour is identical to original (no regression).
        let f = first_filter(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
match_output = [
  { pattern = "Build complete", message = "ok (build complete)" },
]
"#,
        );
        let out = apply_filter(&f, "Build complete!\n");
        assert_eq!(out.trim(), "ok (build complete)");
    }

    #[test]
    fn test_match_output_unless_invalid_regex_rejected() {
        let result = make_filters(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
match_output = [
  { pattern = "success", message = "ok", unless = "[invalid" },
]
"#,
        );
        assert!(result.is_empty());
    }

    // --- replace tests (PR1) ---

    #[test]
    fn test_replace_basic_all_occurrences() {
        let f = first_filter(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
replace = [
  { pattern = "foo", replacement = "bar" },
]
"#,
        );
        let out = apply_filter(&f, "foo baz foo\nfoo");
        assert_eq!(out, "bar baz bar\nbar");
    }

    #[test]
    fn test_replace_chaining_sequential() {
        let f = first_filter(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
replace = [
  { pattern = "aaa", replacement = "bbb" },
  { pattern = "bbb", replacement = "ccc" },
]
"#,
        );
        // Rule 2 operates on the output of rule 1
        let out = apply_filter(&f, "aaa");
        assert_eq!(out, "ccc");
    }

    #[test]
    fn test_replace_backreferences() {
        let f = first_filter(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
replace = [
  { pattern = "(\\w+):(\\w+)", replacement = "$2:$1" },
]
"#,
        );
        let out = apply_filter(&f, "hello:world");
        assert_eq!(out, "world:hello");
    }

    #[test]
    fn test_replace_then_strip_interaction() {
        let f = first_filter(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
replace = [
  { pattern = "noise", replacement = "DROPPED" },
]
strip_lines_matching = ["^DROPPED"]
"#,
        );
        // replace transforms "noise line" -> "DROPPED line", strip removes it
        let out = apply_filter(&f, "noise line\nkeep this");
        assert_eq!(out, "keep this");
    }

    #[test]
    fn test_replace_empty_input_noop() {
        let f = first_filter(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
replace = [
  { pattern = "foo", replacement = "bar" },
]
"#,
        );
        let out = apply_filter(&f, "");
        assert_eq!(out, "");
    }

    #[test]
    fn test_replace_invalid_regex_rejected() {
        let result = make_filters(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
replace = [
  { pattern = "[invalid", replacement = "bar" },
]
"#,
        );
        assert!(result.is_empty());
    }

    // --- verify (PR2) ---

    #[test]
    fn test_run_filter_tests_passes_on_correct_expected() {
        let content = r#"
schema_version = 1

[filters.make]
match_command = "^make\\b"
strip_lines_matching = ["^make\\[\\d+\\]:"]

[[tests.make]]
name = "strips entering/leaving lines"
input = """
make[1]: Entering directory '/home/user'
gcc -O2 foo.c
make[1]: Leaving directory '/home/user'
"""
expected = """
gcc -O2 foo.c
"""
"#;
        let mut outcomes = Vec::new();
        let mut all_names = Vec::new();
        let mut tested = std::collections::HashSet::new();
        collect_test_outcomes(content, None, &mut outcomes, &mut all_names, &mut tested);
        assert_eq!(outcomes.len(), 1);
        assert!(
            outcomes[0].passed,
            "test should pass: {:?}",
            outcomes[0].actual
        );
    }

    #[test]
    fn test_run_filter_tests_fails_on_wrong_expected() {
        let content = r#"
schema_version = 1

[filters.make]
match_command = "^make\\b"
strip_lines_matching = ["^make\\[\\d+\\]:"]

[[tests.make]]
name = "wrong expected"
input = "make[1]: Entering\ngcc foo.c"
expected = "wrong output"
"#;
        let mut outcomes = Vec::new();
        let mut all_names = Vec::new();
        let mut tested = std::collections::HashSet::new();
        collect_test_outcomes(content, None, &mut outcomes, &mut all_names, &mut tested);
        assert_eq!(outcomes.len(), 1);
        assert!(!outcomes[0].passed);
    }

    #[test]
    fn test_filters_without_tests_detected() {
        let content = r#"
schema_version = 1

[filters.make]
match_command = "^make\\b"
"#;
        let mut outcomes = Vec::new();
        let mut all_names = Vec::new();
        let mut tested = std::collections::HashSet::new();
        collect_test_outcomes(content, None, &mut outcomes, &mut all_names, &mut tested);
        // No tests defined, but filter exists
        assert_eq!(outcomes.len(), 0);
        assert!(all_names.contains(&"make".to_string()));
        assert!(!tested.contains("make"));
    }

    // --- Multi-file architecture tests (build.rs) ---

    /// Verify BUILTIN_TOML was generated with the correct schema_version header.
    /// build.rs injects it — if the const is somehow stale this fails immediately.
    #[test]
    fn test_builtin_toml_has_schema_version() {
        assert!(
            BUILTIN_TOML.contains("schema_version = 1"),
            "BUILTIN_TOML must start with 'schema_version = 1' (injected by build.rs)"
        );
    }

    /// Verify every expected filter name is present in BUILTIN_TOML.
    /// This is the safeguard against accidentally deleting a filter file.
    #[test]
    fn test_builtin_all_expected_filters_present() {
        let filters = make_filters(BUILTIN_TOML);
        let names: std::collections::HashSet<&str> =
            filters.iter().map(|f| f.name.as_str()).collect();

        let expected = [
            "ansible-playbook",
            "brew-install",
            "composer-install",
            "df",
            "dotnet-build",
            "du",
            "fail2ban-client",
            "gcloud",
            "hadolint",
            "helm",
            "iptables",
            "liquibase",
            "make",
            "markdownlint",
            "mix-compile",
            "mix-format",
            "mvn-build",
            "ping",
            "pio-run",
            "poetry-install",
            "pre-commit",
            "ps",
            "quarto-render",
            "rsync",
            "shellcheck",
            "shopify-theme",
            "sops",
            "swift-build",
            "systemctl-status",
            "terraform-plan",
            "tofu-fmt",
            "tofu-init",
            "tofu-plan",
            "tofu-validate",
            "trunk-build",
            "uv-sync",
            "yamllint",
        ];

        for name in &expected {
            assert!(
                names.contains(name),
                "Built-in filter '{}' is missing — was its .toml file deleted from src/filters/?",
                name
            );
        }
    }

    /// Verify the exact count of built-in filters.
    /// Fails if a file is added/removed without updating this test.
    #[test]
    fn test_builtin_filter_count() {
        let filters = make_filters(BUILTIN_TOML);
        assert_eq!(
            filters.len(),
            59,
            "Expected exactly 59 built-in filters, got {}. \
             Update this count when adding/removing filters in src/filters/.",
            filters.len()
        );
    }

    /// Verify that every built-in filter has at least one inline test.
    /// Prevents shipping filters with zero test coverage.
    #[test]
    fn test_builtin_all_filters_have_inline_tests() {
        let mut all_names: Vec<String> = Vec::new();
        let mut tested: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut outcomes = Vec::new();
        collect_test_outcomes(
            BUILTIN_TOML,
            None,
            &mut outcomes,
            &mut all_names,
            &mut tested,
        );

        let untested: Vec<&str> = all_names
            .iter()
            .filter(|name| !tested.contains(name.as_str()))
            .map(|s| s.as_str())
            .collect();

        assert!(
            untested.is_empty(),
            "The following built-in filters have no inline tests: {:?}\n\
             Add [[tests.<name>]] entries to the corresponding src/filters/<name>.toml file.",
            untested
        );
    }

    /// Verify that adding a new filter entry to any TOML content makes it
    /// immediately discoverable via find_filter_in — simulating how a new
    /// src/filters/my-tool.toml would work after cargo build.
    #[test]
    fn test_new_filter_discoverable_after_concat() {
        // Simulate build.rs: concat BUILTIN_TOML with a brand-new filter block
        let new_filter = r#"
[filters.my-new-tool]
description = "Compact my-new-tool output"
match_command = "^my-new-tool\\b"
strip_lines_matching = ["^\\s*$"]
max_lines = 30
on_empty = "my-new-tool: ok"

[[tests.my-new-tool]]
name = "strips blank lines"
input = "output line 1\n\noutput line 2"
expected = "output line 1\noutput line 2"
"#;
        let combined = format!("{}\n\n{}", BUILTIN_TOML, new_filter);
        let filters = make_filters(&combined);

        // All 59 existing filters still present + 1 new = 60
        assert_eq!(
            filters.len(),
            60,
            "Expected 60 filters after concat (59 built-in + 1 new)"
        );

        // New filter is discoverable
        let found = find_filter_in("my-new-tool --verbose", &filters);
        assert!(
            found.is_some(),
            "Newly added filter must be discoverable via find_filter_in"
        );
        assert_eq!(found.unwrap().name, "my-new-tool");
    }

    #[test]
    fn test_smart_truncate_keeps_errors_summary() {
        let f = first_filter(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
smart_truncate = 5
"#,
        );
        let input = "\
info: compiling foo
  Downloading dep...
  Using cached dep...
src/bar.rs:42: error: unused variable
  Compiling foo v1.0.0
  Finished in 0.5s
  some random noise
  42 packages installed
  more noise here";
        let out = apply_filter(&f, input);
        assert!(out.contains("error: unused variable"), "should keep error line");
        assert!(out.contains("packages installed"), "should keep summary line");
        assert!(out.contains("more lines]"), "should have omitted-count footer");
        assert!(!out.contains("Downloading"), "should drop download noise");
        assert!(!out.contains("cached"), "should drop cached noise");
        let line_count = out.lines().filter(|l| !l.starts_with('[')).count();
        assert!(line_count <= 4, "keep at most 4 content lines (1 reserved for footer)");
    }

    #[test]
    fn test_smart_truncate_noop_when_under_limit() {
        let f = first_filter(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
smart_truncate = 10
"#,
        );
        let input = "line 1\nline 2\nline 3";
        let out = apply_filter(&f, input);
        assert_eq!(out, "line 1\nline 2\nline 3", "pass through unchanged when under limit");
    }

    #[test]
    fn test_smart_truncate_keeps_warnings() {
        let f = first_filter(
            r#"
schema_version = 1
[filters.f]
match_command = "^cmd"
smart_truncate = 3
"#,
        );
        let input = "\
irrelevant line
WARNING: deprecated feature used
another irrelevant line
last line noise";
        let out = apply_filter(&f, input);
        assert!(out.contains("WARNING"), "should keep warning line");
    }

    // --- coalesce_directory_lines ---

    #[test]
    fn test_coalesce_pairs_basic() {
        let mut lines = vec![
            "make[1]: Entering directory '/home/user/src'".to_string(),
            "gcc -O2 foo.c".to_string(),
            "make[1]: Leaving directory '/home/user/src'".to_string(),
        ];
        coalesce_directory_lines(&mut lines);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "=> /home/user/src");
        assert_eq!(lines[1], "gcc -O2 foo.c");
    }

    #[test]
    fn test_coalesce_nested_pairs() {
        let mut lines = vec![
            "make[1]: Entering directory '/a'".to_string(),
            "make[2]: Entering directory '/a/b'".to_string(),
            "gcc -O2 bar.c".to_string(),
            "make[2]: Leaving directory '/a/b'".to_string(),
            "gcc -O2 foo.c".to_string(),
            "make[1]: Leaving directory '/a'".to_string(),
        ];
        coalesce_directory_lines(&mut lines);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], "=> /a");
        assert_eq!(lines[1], "=> /a/b");
        assert_eq!(lines[2], "gcc -O2 bar.c");
        assert_eq!(lines[3], "gcc -O2 foo.c");
    }

    #[test]
    fn test_coalesce_unmatched_entering_kept() {
        let mut lines = vec![
            "Entering directory '/orphan'".to_string(),
            "some work".to_string(),
        ];
        coalesce_directory_lines(&mut lines);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "Entering directory '/orphan'");
        assert_eq!(lines[1], "some work");
    }

    #[test]
    fn test_coalesce_no_pair_lines_unchanged() {
        let mut lines = vec![
            "normal line".to_string(),
            "another line".to_string(),
        ];
        coalesce_directory_lines(&mut lines);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "normal line");
        assert_eq!(lines[1], "another line");
    }

    #[test]
    fn test_coalesce_pipeline_integration() {
        let f = first_filter(
            r#"
schema_version = 1
[filters.f]
match_command = "^make"
coalesce_directories = true
"#,
        );
        let out = apply_filter(
            &f,
            "make[1]: Entering directory '/home/user/src'\n\
             gcc -O2 foo.c\n\
             make[1]: Leaving directory '/home/user/src'\n\
             Error: build failed",
        );
        assert!(out.contains("=> /home/user/src"));
        assert!(out.contains("gcc -O2 foo.c"));
        assert!(out.contains("Error: build failed"));
    }

    // --- normalize_timestamps_in_line ---

    #[test]
    fn test_normalize_iso_datetime() {
        let mut line = "2024-01-15 10:30:00,123 INFO  Starting".to_string();
        normalize_timestamps_in_line(&mut line);
        assert_eq!(line, "{t} INFO  Starting");

        let mut line2 = "2024-01-15T10:30:00Z Starting".to_string();
        normalize_timestamps_in_line(&mut line2);
        assert_eq!(line2, "{t} Starting");

        let mut line3 = "2024-01-15T10:30:00.123+05:30 verbose".to_string();
        normalize_timestamps_in_line(&mut line3);
        assert_eq!(line3, "{t} verbose");
    }

    #[test]
    fn test_normalize_bracketed_time() {
        let mut line = "[15:30:45] Starting build".to_string();
        normalize_timestamps_in_line(&mut line);
        assert_eq!(line, "{t} Starting build");
    }

    #[test]
    fn test_normalize_duration() {
        let mut line = "Duration: 3.2s".to_string();
        normalize_timestamps_in_line(&mut line);
        assert_eq!(line, "Duration: {t}");

        let mut line2 = "Duration: 123ms".to_string();
        normalize_timestamps_in_line(&mut line2);
        assert_eq!(line2, "Duration: {t}");
    }

    #[test]
    fn test_normalize_time_elapsed() {
        let mut line = "Time Elapsed 00:00:02.34".to_string();
        normalize_timestamps_in_line(&mut line);
        assert_eq!(line, "Time Elapsed {t}");
    }

    #[test]
    fn test_normalize_no_timestamp_unchanged() {
        let mut line = "Building binary release target".to_string();
        normalize_timestamps_in_line(&mut line);
        assert_eq!(line, "Building binary release target");
    }

    #[test]
    fn test_normalize_pipeline_integration() {
        let f = first_filter(
            r#"
schema_version = 1
[filters.f]
match_command = "^build"
normalize_timestamps = true
"#,
        );
        let out = apply_filter(
            &f,
            "2024-01-15 10:30:00,123 INFO  Starting build\n\
             [15:30:45] Compiling foo.rs\n\
             Duration: 3.2s\n\
             Build complete",
        );
        assert_eq!(
            out,
            "{t} INFO  Starting build\n{t} Compiling foo.rs\nDuration: {t}\nBuild complete"
        );
    }
}
