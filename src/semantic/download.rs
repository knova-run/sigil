//! Download the `potion-code-16M` static-embedding model to the cache
//! directory used by `sigil semantic --m2v`.
//!
//! `sigil semantic-download-model` fetches three files from Hugging Face:
//!
//!   config.json
//!   tokenizer.json
//!   model.safetensors
//!
//! into `$XDG_CACHE_HOME/sigil/models/potion-code-16M/` (resolved via
//! `dirs::cache_dir()`). Idempotent: existing files are kept unless
//! `--force` is set. The downloader streams response bodies so the
//! 60 MB model.safetensors doesn't peak RAM, and emits stderr progress
//! every 200 ms in the same format as the eager-build pass:
//!
//!   download: model.safetensors 23/64 MB (35%)
//!
//! No SHA pinning yet — HF doesn't expose a stable per-file SHA via the
//! `/resolve/main/` URL space. A follow-up will validate against the
//! per-commit content hash exposed by the model card JSON.

use anyhow::{anyhow, Context, Result};
use std::fs::File;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Files the downloader fetches, in order. `model.safetensors` is the big
/// one (~60 MB); the others are tiny (config: <1 KB, tokenizer: ~1 MB).
pub const MODEL_FILES: &[&str] = &["config.json", "tokenizer.json", "model.safetensors"];

/// Default upstream base URL. The `/resolve/main/<file>` shape mirrors
/// what curl-from-the-error-message gives users today.
pub const HF_BASE_URL: &str =
    "https://huggingface.co/minishlab/potion-code-16M/resolve/main";

/// Default model name (matches `m2v.rs::default_model_dir`).
pub const MODEL_NAME: &str = "potion-code-16M";

/// What `download_model_with` should do for each file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAction {
    /// File already on disk; left alone (force=false).
    Skipped,
    /// File downloaded fresh.
    Downloaded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadStats {
    pub skipped: usize,
    pub downloaded: usize,
    pub bytes: u64,
}

/// Pure URL composition. Trailing slash on `base` is tolerated.
pub fn file_url(base: &str, filename: &str) -> String {
    let base = base.trim_end_matches('/');
    format!("{base}/{filename}")
}

/// Local path for a downloaded file. Mirrors the structure
/// `m2v.rs::default_model_dir` resolves to.
pub fn file_dest(dir: &Path, filename: &str) -> PathBuf {
    dir.join(filename)
}

/// Whether to skip downloading this file: present + non-empty + not forced.
pub fn should_skip(dest: &Path, force: bool) -> bool {
    if force {
        return false;
    }
    match dest.metadata() {
        Ok(m) if m.is_file() && m.len() > 0 => true,
        _ => false,
    }
}

/// Stream a reader into `dest`, emitting progress via `on_progress`
/// (called with `(bytes_so_far, content_length_hint)`). Atomically
/// renames a `.partial` to `dest` on success so a crash mid-download
/// leaves the cache in a known state.
pub fn stream_to_file<R: Read>(
    reader: R,
    dest: &Path,
    content_length: Option<u64>,
    on_progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<u64> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create dir {}", parent.display()))?;
    }
    let partial = dest.with_extension(format!(
        "{}.partial",
        dest.extension().and_then(|s| s.to_str()).unwrap_or("")
    ));
    let mut file = File::create(&partial)
        .with_context(|| format!("create {}", partial.display()))?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut so_far: u64 = 0;
    let mut reader = reader;
    loop {
        let n = reader
            .read(&mut buf)
            .with_context(|| format!("read for {}", dest.display()))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .with_context(|| format!("write {}", partial.display()))?;
        so_far += n as u64;
        on_progress(so_far, content_length);
    }
    file.flush().ok();
    drop(file);
    std::fs::rename(&partial, dest).with_context(|| {
        format!(
            "rename {} -> {}",
            partial.display(),
            dest.display()
        )
    })?;
    Ok(so_far)
}

/// HTTP-fetch boundary. Production wires this to `ureq`; tests wire it
/// to a local server. Returns an open reader plus an optional
/// content-length hint.
pub trait Fetcher {
    fn get(&self, url: &str) -> Result<(Box<dyn Read + Send>, Option<u64>)>;
}

/// Drives the full multi-file download. `fetcher` lets tests substitute
/// the HTTP transport.
pub fn download_model_with<F: Fetcher>(
    dir: &Path,
    base_url: &str,
    force: bool,
    fetcher: &F,
    on_file_start: &mut dyn FnMut(&str, Option<u64>),
    on_progress: &mut dyn FnMut(&str, u64, Option<u64>),
    on_file_done: &mut dyn FnMut(&str, FileAction, u64),
) -> Result<DownloadStats> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create model dir {}", dir.display()))?;
    let mut stats = DownloadStats {
        skipped: 0,
        downloaded: 0,
        bytes: 0,
    };
    for &filename in MODEL_FILES {
        let dest = file_dest(dir, filename);
        if should_skip(&dest, force) {
            stats.skipped += 1;
            on_file_done(filename, FileAction::Skipped, 0);
            continue;
        }
        let url = file_url(base_url, filename);
        let (reader, content_len) = fetcher
            .get(&url)
            .with_context(|| format!("GET {url}"))?;
        on_file_start(filename, content_len);
        let bytes = stream_to_file(reader, &dest, content_len, &mut |so_far, total| {
            on_progress(filename, so_far, total);
        })?;
        stats.downloaded += 1;
        stats.bytes += bytes;
        on_file_done(filename, FileAction::Downloaded, bytes);
    }
    Ok(stats)
}

// --- Production wrapper: ureq Fetcher + stderr progress -------------------

struct UreqFetcher;

impl Fetcher for UreqFetcher {
    fn get(&self, url: &str) -> Result<(Box<dyn Read + Send>, Option<u64>)> {
        let resp = ureq::get(url)
            .timeout(Duration::from_secs(60))
            .call()
            .map_err(|e| anyhow!("ureq GET {url}: {e}"))?;
        let content_len = resp
            .header("Content-Length")
            .and_then(|s| s.parse::<u64>().ok());
        Ok((Box::new(resp.into_reader()), content_len))
    }
}

/// CLI entry point: download to the conventional cache dir, with stderr
/// progress when `verbose` is set.
pub fn run(dir: Option<PathBuf>, base_url: Option<String>, force: bool, verbose: bool) -> Result<()> {
    let dir = dir
        .or_else(crate::semantic::m2v::default_model_dir)
        .ok_or_else(|| anyhow!("could not resolve user cache dir"))?;
    let base_url = base_url.unwrap_or_else(|| HF_BASE_URL.to_string());
    let stderr_tty = std::io::stderr().is_terminal();

    let mut last_print = Instant::now()
        .checked_sub(Duration::from_millis(500))
        .unwrap_or_else(Instant::now);
    let mut wrote_tty_line = false;

    let mut on_file_start = |filename: &str, total: Option<u64>| {
        if !verbose {
            return;
        }
        let total_mb = total.map(bytes_to_mb).unwrap_or(0.0);
        eprintln!("download: {filename} (start, ~{total_mb:.1} MB)");
    };
    let mut on_progress = |filename: &str, so_far: u64, total: Option<u64>| {
        if !verbose {
            return;
        }
        let is_last = total.map(|t| so_far == t).unwrap_or(false);
        if !is_last && last_print.elapsed() < Duration::from_millis(200) {
            return;
        }
        last_print = Instant::now();
        let so_far_mb = bytes_to_mb(so_far);
        let total_mb = total.map(bytes_to_mb);
        let pct = total
            .filter(|t| *t > 0)
            .map(|t| 100.0 * so_far as f64 / t as f64);
        let pct_s = pct.map(|p| format!(" ({p:5.1}%)")).unwrap_or_default();
        let total_s = total_mb
            .map(|t| format!("/{t:.1} MB"))
            .unwrap_or_default();
        if stderr_tty {
            eprint!(
                "\rdownload: {filename} {so_far_mb:6.1}{total_s}{pct_s}             "
            );
            let _ = std::io::stderr().flush();
            wrote_tty_line = true;
            if is_last {
                eprintln!();
                wrote_tty_line = false;
            }
        } else {
            eprintln!("download: {filename} {so_far_mb:.1}{total_s}{pct_s}");
        }
    };
    let mut on_file_done = |filename: &str, action: FileAction, bytes: u64| {
        if !verbose && action == FileAction::Skipped {
            return;
        }
        match action {
            FileAction::Skipped => eprintln!("download: {filename} skipped (cached, --force to refresh)"),
            FileAction::Downloaded => {
                let mb = bytes_to_mb(bytes);
                eprintln!("download: {filename} OK ({mb:.1} MB)");
            }
        }
    };

    let stats = download_model_with(
        &dir,
        &base_url,
        force,
        &UreqFetcher,
        &mut on_file_start,
        &mut on_progress,
        &mut on_file_done,
    )?;
    if verbose && wrote_tty_line {
        eprintln!();
    }
    let total_mb = bytes_to_mb(stats.bytes);
    eprintln!(
        "Model {MODEL_NAME}: {} downloaded, {} cached → {} ({:.1} MB this run)",
        stats.downloaded,
        stats.skipped,
        dir.display(),
        total_mb
    );
    Ok(())
}

fn bytes_to_mb(b: u64) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    // --- pure helpers --------------------------------------------------

    #[test]
    fn file_url_joins_base_and_filename() {
        assert_eq!(
            file_url("https://hf/x/resolve/main", "config.json"),
            "https://hf/x/resolve/main/config.json"
        );
    }

    #[test]
    fn file_url_tolerates_trailing_slash() {
        assert_eq!(
            file_url("https://hf/x/resolve/main/", "model.safetensors"),
            "https://hf/x/resolve/main/model.safetensors"
        );
    }

    #[test]
    fn file_dest_under_cache_dir() {
        let d = Path::new("/cache/sigil/models/potion-code-16M");
        assert_eq!(
            file_dest(d, "model.safetensors"),
            Path::new("/cache/sigil/models/potion-code-16M/model.safetensors")
        );
    }

    #[test]
    fn should_skip_returns_true_when_file_exists_and_force_false() {
        let dir = std::env::temp_dir().join(format!("sigil-dl-skip-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("present");
        std::fs::write(&f, b"hello").unwrap();
        assert!(should_skip(&f, false));
        assert!(!should_skip(&f, true), "--force should override skip");
        std::fs::remove_file(&f).ok();
    }

    #[test]
    fn should_skip_returns_false_when_file_missing() {
        let dir = std::env::temp_dir().join(format!("sigil-dl-miss-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("absent");
        std::fs::remove_file(&f).ok();
        assert!(!should_skip(&f, false));
    }

    #[test]
    fn should_skip_returns_false_when_file_empty() {
        let dir = std::env::temp_dir().join(format!("sigil-dl-empty-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("empty");
        std::fs::write(&f, b"").unwrap();
        assert!(!should_skip(&f, false), "empty file should not count as cached");
        std::fs::remove_file(&f).ok();
    }

    // --- stream_to_file --------------------------------------------------

    #[test]
    fn stream_to_file_writes_full_payload() {
        let dir = std::env::temp_dir().join(format!("sigil-dl-stream-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let dest = dir.join("payload.bin");
        std::fs::remove_file(&dest).ok();
        let payload = vec![42u8; 200_000];
        let bytes = stream_to_file(
            Cursor::new(payload.clone()),
            &dest,
            Some(payload.len() as u64),
            &mut |_so_far, _total| {},
        )
        .unwrap();
        assert_eq!(bytes, payload.len() as u64);
        let written = std::fs::read(&dest).unwrap();
        assert_eq!(written, payload);
        std::fs::remove_file(&dest).ok();
    }

    #[test]
    fn stream_to_file_invokes_progress_at_least_once_per_chunk() {
        let dir = std::env::temp_dir().join(format!("sigil-dl-prog-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let dest = dir.join("p.bin");
        std::fs::remove_file(&dest).ok();
        // ~150 KB → >2 chunks of 64 KB → at least 3 progress callbacks.
        let payload = vec![7u8; 150_000];
        let mut last_seen: u64 = 0;
        let mut calls = 0u32;
        stream_to_file(
            Cursor::new(payload.clone()),
            &dest,
            Some(payload.len() as u64),
            &mut |so_far, _total| {
                calls += 1;
                last_seen = so_far;
            },
        )
        .unwrap();
        assert!(calls >= 2, "expected at least 2 progress ticks, got {calls}");
        assert_eq!(last_seen, payload.len() as u64, "last tick should equal total");
        std::fs::remove_file(&dest).ok();
    }

    // --- download_model_with using a stub Fetcher -----------------------

    struct StubFetcher {
        bodies: std::collections::HashMap<String, Vec<u8>>,
    }
    impl Fetcher for StubFetcher {
        fn get(&self, url: &str) -> Result<(Box<dyn Read + Send>, Option<u64>)> {
            let body = self
                .bodies
                .get(url)
                .ok_or_else(|| anyhow!("404: {url}"))?
                .clone();
            let len = body.len() as u64;
            Ok((Box::new(Cursor::new(body)), Some(len)))
        }
    }

    fn stub_with_three_files() -> StubFetcher {
        let mut m = std::collections::HashMap::new();
        m.insert(
            "http://test/config.json".to_string(),
            br#"{"normalize":true}"#.to_vec(),
        );
        m.insert(
            "http://test/tokenizer.json".to_string(),
            br#"{"version":"1.0"}"#.to_vec(),
        );
        m.insert("http://test/model.safetensors".to_string(), vec![0u8; 64]);
        StubFetcher { bodies: m }
    }

    #[test]
    fn download_model_with_writes_all_three_files() {
        let dir =
            std::env::temp_dir().join(format!("sigil-dl-3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut start = |_: &str, _: Option<u64>| {};
        let mut prog = |_: &str, _: u64, _: Option<u64>| {};
        let mut done = |_: &str, _: FileAction, _: u64| {};
        let stats = download_model_with(
            &dir,
            "http://test",
            false,
            &stub_with_three_files(),
            &mut start,
            &mut prog,
            &mut done,
        )
        .unwrap();
        assert_eq!(stats.downloaded, 3);
        assert_eq!(stats.skipped, 0);
        for f in MODEL_FILES {
            assert!(dir.join(f).exists(), "missing {f}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_model_with_is_idempotent() {
        let dir =
            std::env::temp_dir().join(format!("sigil-dl-idem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let stub = stub_with_three_files();
        let mut start = |_: &str, _: Option<u64>| {};
        let mut prog = |_: &str, _: u64, _: Option<u64>| {};
        let mut done = |_: &str, _: FileAction, _: u64| {};
        let _ = download_model_with(&dir, "http://test", false, &stub, &mut start, &mut prog, &mut done).unwrap();
        let stats = download_model_with(&dir, "http://test", false, &stub, &mut start, &mut prog, &mut done).unwrap();
        assert_eq!(stats.downloaded, 0, "second run should hit the cache");
        assert_eq!(stats.skipped, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_model_with_force_redownloads_everything() {
        let dir =
            std::env::temp_dir().join(format!("sigil-dl-force-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let stub = stub_with_three_files();
        let mut start = |_: &str, _: Option<u64>| {};
        let mut prog = |_: &str, _: u64, _: Option<u64>| {};
        let mut done = |_: &str, _: FileAction, _: u64| {};
        let _ = download_model_with(&dir, "http://test", false, &stub, &mut start, &mut prog, &mut done).unwrap();
        let stats = download_model_with(&dir, "http://test", true, &stub, &mut start, &mut prog, &mut done).unwrap();
        assert_eq!(stats.downloaded, 3, "force should re-fetch all three");
        assert_eq!(stats.skipped, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_model_with_returns_error_on_404() {
        let dir =
            std::env::temp_dir().join(format!("sigil-dl-404-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let stub = StubFetcher {
            bodies: std::collections::HashMap::new(),
        };
        let mut start = |_: &str, _: Option<u64>| {};
        let mut prog = |_: &str, _: u64, _: Option<u64>| {};
        let mut done = |_: &str, _: FileAction, _: u64| {};
        let err = download_model_with(&dir, "http://test", false, &stub, &mut start, &mut prog, &mut done)
            .expect_err("expected 404 error");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("config.json") || msg.contains("404"),
            "error message should reference failure: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
