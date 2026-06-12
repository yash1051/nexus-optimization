//! Delta-read cache: avoids re-sending file content the AI has already seen.
//!
//! On repeated reads of the same file (same filter/window options):
//! - unchanged file  -> one-line "unchanged" notice instead of full content
//! - changed file    -> unified diff against the previously sent content
//!
//! Both are lossless for the consuming agent: the prior content is already in
//! its context, so "unchanged" or "prior + diff" carries the same information
//! as a full re-send. A TTL bounds staleness across sessions, and the notice
//! always names the escape hatch (`--no-cache`) for full content.

use super::constants::RTK_DATA_DIR;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// Files larger than this are never cached (diffing cost outweighs savings).
const MAX_CACHED_BYTES: usize = 2 * 1024 * 1024;
/// If the diff isn't at least this much smaller than the full output, send full.
const DIFF_WORTHWHILE_RATIO: f64 = 0.6;
/// LCS DP guard: beyond this many changed lines per side, fall back to full output.
const MAX_DIFF_LINES: usize = 3000;

#[derive(Debug, Serialize, Deserialize)]
struct CacheEntry {
    /// Unix timestamp (seconds) of when this content was last sent in full/diff.
    ts: i64,
    content: String,
}

pub enum ReadCacheResult {
    /// No usable cache entry; print full output.
    Miss,
    /// Identical to what was already sent; print a one-line notice.
    Unchanged { age_minutes: i64, lines: usize },
    /// Changed; print this pre-formatted notice + unified diff instead.
    Diff { rendered: String },
}

pub struct ReadCache {
    dir: PathBuf,
    ttl_minutes: i64,
    enabled: bool,
}

impl ReadCache {
    pub fn from_config() -> Self {
        let cfg = super::config::Config::load().unwrap_or_default();
        let enabled = cfg.read_cache.enabled && std::env::var_os("RTK_NO_READ_CACHE").is_none();
        Self {
            dir: cache_dir(),
            ttl_minutes: cfg.read_cache.ttl_minutes as i64,
            enabled,
        }
    }

    #[cfg(test)]
    fn for_test(dir: PathBuf, ttl_minutes: i64) -> Self {
        Self {
            dir,
            ttl_minutes,
            enabled: true,
        }
    }

    /// Compare `output` against the cached copy under `key`, store the new
    /// output, and report what should be printed. Any I/O failure degrades to
    /// `Miss` — the cache must never break a read.
    pub fn check_and_update(&self, key: &str, display_path: &str, output: &str) -> ReadCacheResult {
        if !self.enabled || output.len() > MAX_CACHED_BYTES {
            return ReadCacheResult::Miss;
        }

        let path = self.dir.join(format!("{}.json", hash_key(key)));
        let now = chrono::Utc::now().timestamp();

        let previous: Option<CacheEntry> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok());

        let result = match previous {
            Some(entry) if now - entry.ts <= self.ttl_minutes * 60 => {
                let age_minutes = (now - entry.ts) / 60;
                if entry.content == output {
                    ReadCacheResult::Unchanged {
                        age_minutes,
                        lines: output.lines().count(),
                    }
                } else {
                    match render_diff(&entry.content, output, display_path, age_minutes) {
                        Some(rendered) => ReadCacheResult::Diff { rendered },
                        None => ReadCacheResult::Miss,
                    }
                }
            }
            // Expired or absent: treat as a fresh read.
            _ => ReadCacheResult::Miss,
        };

        let entry = CacheEntry {
            ts: now,
            content: output.to_string(),
        };
        if std::fs::create_dir_all(&self.dir).is_ok() {
            if let Ok(json) = serde_json::to_string(&entry) {
                let _ = std::fs::write(&path, json);
            }
        }

        result
    }
}

fn cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(RTK_DATA_DIR)
        .join("read-cache")
}

fn hash_key(key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    let digest = hasher.finalize();
    // 16 hex chars is plenty for a local, non-adversarial cache.
    digest[..8].iter().map(|b| format!("{:02x}", b)).collect()
}

/// Build the full replacement output (notice + unified diff), or `None` when
/// a diff would not be worthwhile (too large or barely smaller than full).
fn render_diff(old: &str, new: &str, display_path: &str, age_minutes: i64) -> Option<String> {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();

    // Trim common prefix/suffix so the DP only sees the changed region.
    let mut start = 0;
    while start < old_lines.len()
        && start < new_lines.len()
        && old_lines[start] == new_lines[start]
    {
        start += 1;
    }
    let mut old_end = old_lines.len();
    let mut new_end = new_lines.len();
    while old_end > start && new_end > start && old_lines[old_end - 1] == new_lines[new_end - 1] {
        old_end -= 1;
        new_end -= 1;
    }

    let old_mid = &old_lines[start..old_end];
    let new_mid = &new_lines[start..new_end];
    if old_mid.len() > MAX_DIFF_LINES || new_mid.len() > MAX_DIFF_LINES {
        return None;
    }

    let ops = lcs_diff(old_mid, new_mid);

    let mut body = String::new();
    body.push_str(&format!(
        "@@ -{},{} +{},{} @@\n",
        start + 1,
        old_mid.len(),
        start + 1,
        new_mid.len()
    ));
    for op in &ops {
        match op {
            DiffOp::Del(line) => {
                body.push('-');
                body.push_str(line);
                body.push('\n');
            }
            DiffOp::Add(line) => {
                body.push('+');
                body.push_str(line);
                body.push('\n');
            }
            DiffOp::Same(line) => {
                body.push(' ');
                body.push_str(line);
                body.push('\n');
            }
        }
    }

    let header = format!(
        "[nexus delta] {} changed since last read {}m ago — diff vs. content already in your context (full file: nexus read --no-cache {})\n",
        display_path, age_minutes, display_path
    );
    let rendered = format!("{}{}", header, body);

    if (rendered.len() as f64) < (new.len() as f64) * DIFF_WORTHWHILE_RATIO {
        Some(rendered)
    } else {
        None
    }
}

enum DiffOp<'a> {
    Same(&'a str),
    Del(&'a str),
    Add(&'a str),
}

/// Classic DP LCS over the (already prefix/suffix-trimmed) changed region.
fn lcs_diff<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<DiffOp<'a>> {
    let n = old.len();
    let m = new.len();
    // (n+1) x (m+1) table of LCS lengths.
    let mut table = vec![0u32; (n + 1) * (m + 1)];
    let idx = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[idx(i, j)] = if old[i] == new[j] {
                table[idx(i + 1, j + 1)] + 1
            } else {
                table[idx(i + 1, j)].max(table[idx(i, j + 1)])
            };
        }
    }

    let mut ops = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if old[i] == new[j] {
            ops.push(DiffOp::Same(old[i]));
            i += 1;
            j += 1;
        } else if table[idx(i + 1, j)] >= table[idx(i, j + 1)] {
            ops.push(DiffOp::Del(old[i]));
            i += 1;
        } else {
            ops.push(DiffOp::Add(new[j]));
            j += 1;
        }
    }
    while i < n {
        ops.push(DiffOp::Del(old[i]));
        i += 1;
    }
    while j < m {
        ops.push(DiffOp::Add(new[j]));
        j += 1;
    }
    ops
}

/// One-line notice for an unchanged repeat read.
pub fn unchanged_notice(display_path: &str, age_minutes: i64, lines: usize) -> String {
    format!(
        "[nexus delta] {} unchanged since last read {}m ago ({} lines, already in your context — full file: nexus read --no-cache {})\n",
        display_path, age_minutes, lines, display_path
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn cache(dir: &TempDir) -> ReadCache {
        ReadCache::for_test(dir.path().to_path_buf(), 240)
    }

    #[test]
    fn first_read_is_miss() {
        let dir = TempDir::new().unwrap();
        let c = cache(&dir);
        assert!(matches!(
            c.check_and_update("k", "f.rs", "hello\n"),
            ReadCacheResult::Miss
        ));
    }

    #[test]
    fn second_identical_read_is_unchanged() {
        let dir = TempDir::new().unwrap();
        let c = cache(&dir);
        c.check_and_update("k", "f.rs", "hello\nworld\n");
        match c.check_and_update("k", "f.rs", "hello\nworld\n") {
            ReadCacheResult::Unchanged { lines, .. } => assert_eq!(lines, 2),
            _ => panic!("expected Unchanged"),
        }
    }

    #[test]
    fn changed_read_yields_diff_for_small_edit() {
        let dir = TempDir::new().unwrap();
        let c = cache(&dir);
        let old: String = (0..100).map(|i| format!("line {}\n", i)).collect();
        let new = old.replace("line 50\n", "line fifty\n");
        c.check_and_update("k", "f.rs", &old);
        match c.check_and_update("k", "f.rs", &new) {
            ReadCacheResult::Diff { rendered } => {
                assert!(rendered.contains("-line 50"));
                assert!(rendered.contains("+line fifty"));
                assert!(rendered.len() < new.len() / 2, "diff must be small");
            }
            _ => panic!("expected Diff"),
        }
    }

    #[test]
    fn rewrite_of_whole_file_falls_back_to_full() {
        let dir = TempDir::new().unwrap();
        let c = cache(&dir);
        let old: String = (0..50).map(|i| format!("alpha {}\n", i)).collect();
        let new: String = (0..50).map(|i| format!("omega {}\n", i)).collect();
        c.check_and_update("k", "f.rs", &old);
        // Diff would be ~2x the file: not worthwhile, so Miss (full output).
        assert!(matches!(
            c.check_and_update("k", "f.rs", &new),
            ReadCacheResult::Miss
        ));
    }

    #[test]
    fn expired_entry_is_miss() {
        let dir = TempDir::new().unwrap();
        let c = ReadCache::for_test(dir.path().to_path_buf(), 0);
        c.check_and_update("k", "f.rs", "hello\n");
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert!(matches!(
            c.check_and_update("k", "f.rs", "hello\n"),
            ReadCacheResult::Miss
        ));
    }

    #[test]
    fn distinct_keys_do_not_collide() {
        let dir = TempDir::new().unwrap();
        let c = cache(&dir);
        c.check_and_update("a.rs|none", "a.rs", "content\n");
        assert!(matches!(
            c.check_and_update("b.rs|none", "b.rs", "content\n"),
            ReadCacheResult::Miss
        ));
    }

    #[test]
    fn oversized_output_is_never_cached() {
        let dir = TempDir::new().unwrap();
        let c = cache(&dir);
        let big = "x".repeat(MAX_CACHED_BYTES + 1);
        assert!(matches!(
            c.check_and_update("k", "f.rs", &big),
            ReadCacheResult::Miss
        ));
        assert!(matches!(
            c.check_and_update("k", "f.rs", &big),
            ReadCacheResult::Miss
        ));
    }

    #[test]
    fn unchanged_notice_mentions_escape_hatch() {
        let n = unchanged_notice("src/main.rs", 5, 100);
        assert!(n.contains("--no-cache"));
        assert!(n.contains("src/main.rs"));
    }
}
