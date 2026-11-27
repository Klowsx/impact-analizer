//! Reproduce Laine's analysis and indexing latency and peak Rust heap usage.

use std::alloc::{GlobalAlloc, Layout, System};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use clap::{Parser, ValueEnum};
use laine_cli::engine::{AnalyzeRequest, index_workspace, run_analysis_with_options};
use laine_cli::report::render_json;
use laine_core::model::{AnalysisReport, Confidence, ContractChange};
use serde::Serialize;

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const MB: f64 = 1_000_000.0;

struct TrackingAllocator {
    current_bytes: AtomicUsize,
    peak_bytes: AtomicUsize,
}

impl TrackingAllocator {
    // Record the high-water mark after atomically increasing live allocations.
    fn add(&self, bytes: usize) {
        let live = self.current_bytes.fetch_add(bytes, Relaxed) + bytes;
        self.peak_bytes.fetch_max(live, Relaxed);
    }

    // Start each phase with its existing live heap as the baseline.
    fn reset_peak(&self) {
        self.peak_bytes
            .store(self.current_bytes.load(Relaxed), Relaxed);
    }
}

// SAFETY: Every operation delegates allocation and layout handling to System.
unsafe impl GlobalAlloc for TrackingAllocator {
    // Count the requested bytes only once System returns a live allocation.
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: The caller supplies a valid layout for GlobalAlloc::alloc.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            self.add(layout.size());
        }
        ptr
    }

    // Remove released bytes from the live allocation count.
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.current_bytes.fetch_sub(layout.size(), Relaxed);
        // SAFETY: ptr was allocated by System with this layout.
        unsafe { System.dealloc(ptr, layout) };
    }

    // Preserve the old allocation count if System fails to resize it.
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: ptr, layout, and new_size satisfy GlobalAlloc::realloc's contract.
        let ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if !ptr.is_null() {
            if new_size > layout.size() {
                self.add(new_size - layout.size());
            } else {
                self.current_bytes
                    .fetch_sub(layout.size() - new_size, Relaxed);
            }
        }
        ptr
    }
}

#[global_allocator]
static ALLOCATOR: TrackingAllocator = TrackingAllocator {
    current_bytes: AtomicUsize::new(0),
    peak_bytes: AtomicUsize::new(0),
};

#[derive(Clone, Copy, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
enum Scale {
    Small,
    Medium,
    Large,
}

impl Scale {
    // Return the number of repos and tracked files in the generated workspace.
    fn size(self) -> (usize, usize) {
        match self {
            Self::Small => (4, 40),
            Self::Medium => (4, 200),
            Self::Large => (6, 500),
        }
    }
}

#[derive(Parser)]
#[command(about = "Benchmark deterministic Laine analysis and indexing phases")]
struct Args {
    #[arg(long, value_enum, default_value = "small")]
    scale: Scale,
    #[arg(long, default_value = "3", value_parser = clap::value_parser!(u32).range(1..))]
    iterations: u32,
    #[arg(long, conflicts_with = "markdown")]
    json: bool,
    #[arg(long)]
    markdown: bool,
}

struct Scratch(PathBuf);

impl Scratch {
    // Reserve a private directory for generated repos and SQLite databases.
    fn new() -> Result<Self> {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let seq = NEXT_ID.fetch_add(1, Relaxed);
        let path =
            std::env::temp_dir().join(format!("laine-bench-{}-{stamp}-{seq}", std::process::id()));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }
}

impl Drop for Scratch {
    // Clean up the generated workspace even if a benchmark phase fails.
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            eprintln!("Could not remove {}: {error}", self.0.display());
        }
    }
}

// Run Git with fixed commit identity and dates without changing user config.
fn git(repo: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .current_dir(repo)
        .args([
            "-c",
            "core.autocrlf=false",
            "-c",
            "core.hooksPath=.git/no-hooks",
        ])
        .args(args)
        .env("GIT_AUTHOR_NAME", "Laine Benchmark")
        .env("GIT_AUTHOR_EMAIL", "bench@example.invalid")
        .env("GIT_COMMITTER_NAME", "Laine Benchmark")
        .env("GIT_COMMITTER_EMAIL", "bench@example.invalid")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "git {args:?} in {}: {}",
            repo.display(),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(())
}

// Stage and commit the generated files or the single-file benchmark edit.
fn commit(repo: &Path, message: &str) -> Result<()> {
    git(repo, &["add", "--all"])?;
    git(repo, &["commit", "--quiet", "--no-gpg-sign", "-m", message])
}

const SPEC: &str = "openapi: 3.0.3\npaths:\n  /users:\n    get:\n      responses:\n        '200':\n          description: Users\n          content:\n            application/json:\n              schema:\n                type: object\n                properties:\n                  id: {type: integer}\n";
const SERVICE: &str = "import { api } from './index';\nimport { UserSchema } from './schema';\nimport type { User } from './dto';\nexport async function load() {\n  const response = await api.get<User>('/users');\n  return UserSchema.parse(response.data);\n}\n";
const LEAF: &str = "import { load } from './service';\nexport async function Card() {\n  const user = await load();\n  return <span>{user.displayName}</span>;\n}\n";
const FIXTURES: [(&str, &str); 5] = [
    (
        "client.ts",
        "import axios from 'axios';\nexport const api = axios.create({ baseURL: 'http://api' });\n",
    ),
    ("index.ts", "export { api } from './client';\n"),
    (
        "schema.ts",
        "import { z } from 'zod';\nexport const UserSchema = z.object({id: z.number(), displayName: z.string()});\n",
    ),
    (
        "dto.ts",
        "export interface User { id: number; displayName: string }\n",
    ),
    ("service.ts", SERVICE),
];

// Build one producer and mixed-language consumer repos with shared service imports.
fn generate(root: &Path, repos: usize, files: usize) -> Result<AnalyzeRequest> {
    let api = root.join("api");
    fs::create_dir(&api)?;
    git(
        &api,
        &[
            "init",
            "--quiet",
            "--initial-branch=main",
            "--object-format=sha1",
        ],
    )?;
    fs::write(
        api.join("openapi.yaml"),
        format!("{SPEC}                  displayName: {{type: string}}\n"),
    )?;
    commit(&api, "Add user response")?;
    fs::write(api.join("openapi.yaml"), SPEC)?;
    commit(&api, "Remove display name")?;

    let mut config = String::from(
        "version = 1\ncache_path = 'index.sqlite3'\n\n[[services]]\nname = 'api'\npath = './api'\nopenapi = 'openapi.yaml'\nbase_urls = ['http://api']\n",
    );
    let generated = files - 1 - (repos - 1) * FIXTURES.len();
    for index in 0..repos - 1 {
        let name = format!("consumer-{index}");
        let repo = root.join(&name);
        fs::create_dir(&repo)?;
        git(
            &repo,
            &[
                "init",
                "--quiet",
                "--initial-branch=main",
                "--object-format=sha1",
            ],
        )?;
        config.push_str(&format!(
            "\n[[services]]\nname = '{name}'\npath = './{name}'\n"
        ));
        for (file, source) in FIXTURES {
            fs::write(repo.join(file), source)?;
        }
        let repo_files = generated / (repos - 1) + usize::from(index < generated % (repos - 1));
        for module in 0..repo_files {
            let (extension, source) = match module % 4 {
                0 => ("tsx", LEAF),
                1 => (
                    "vue",
                    "<template>\n  <span>{{ user.displayName }}</span>\n</template>\n<script setup lang=\"ts\">\nimport { ref } from 'vue';\nimport { load } from './service';\nconst user = ref(null);\nasync function init() { user.value = await load(); }\n</script>\n",
                ),
                2 => (
                    "py",
                    "import requests\n\ndef load_user():\n    response = requests.get('http://api/users')\n    return response.json()['displayName']\n",
                ),
                _ => (
                    "py",
                    "import httpx\n\ndef load_user():\n    client = httpx.Client(base_url='http://api')\n    response = client.get('/users')\n    return response.json()['displayName']\n",
                ),
            };
            fs::write(repo.join(format!("view-{module:03}.{extension}")), source)?;
        }
        commit(&repo, "Add consumer modules")?;
    }
    let config_path = root.join("laine.toml");
    fs::write(&config_path, config)?;
    Ok(AnalyzeRequest {
        config_path,
        target_repo: "api".into(),
        base_rev: "HEAD^".into(),
        head_rev: "HEAD".into(),
    })
}

#[derive(Clone, Copy)]
struct Sample {
    ms: f64,
    peak_heap_mb: f64,
    os_rss_hwm_mb: Option<f64>,
}

// Read Linux's process-wide RSS high-water mark after the timed operation.
fn os_rss_hwm_mb() -> Option<f64> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let kib: u64 = status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some(kib as f64 * 1024.0 / MB)
}

// Measure a synchronous engine operation before inspecting or serializing its result.
fn measure<T>(operation: impl FnOnce() -> laine_core::error::Result<T>) -> Result<(T, Sample)> {
    ALLOCATOR.reset_peak();
    let start = Instant::now();
    let result = operation();
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    let peak_heap_mb = ALLOCATOR.peak_bytes.load(Relaxed) as f64 / MB;
    Ok((
        result?,
        Sample {
            ms,
            peak_heap_mb,
            os_rss_hwm_mb: os_rss_hwm_mb(),
        },
    ))
}

const PHASES: [&str; 7] = [
    "cold_no_cache",
    "cold_populate_cache",
    "warm_full_cache_hit",
    "incremental_1file_leaf_commit",
    "incremental_1file_shared_service",
    "index_cold",
    "index_warm",
];

// Compare full JSON at the same revision.
fn verify(report: &AnalysisReport, expected_json: &str, phase: &str) -> Result<()> {
    if render_json(report)? != expected_json {
        return Err(
            format!("{phase} differs from the uncached report at the same revisions").into(),
        );
    }
    Ok(())
}

// Replace the edited repo's current revision in the known report metadata fields.
fn normalize_edit(report: &mut AnalysisReport, previous: &AnalysisReport) -> Result<()> {
    let current = &report.consumer_revisions["consumer-0"];
    let base = &previous.consumer_revisions["consumer-0"];
    if current == base {
        return Err("incremental analysis did not see a new consumer revision".into());
    }
    for finding in &mut report.findings {
        for hit in &mut finding.consumers {
            if hit.repo == "consumer-0" {
                if &hit.evidence.revision != current {
                    return Err("incremental analysis returned stale consumer evidence".into());
                }
                hit.evidence.revision.clone_from(base);
            }
        }
    }
    report
        .consumer_revisions
        .insert("consumer-0".into(), base.clone());
    Ok(())
}

// Time all phases and verify cache behavior and JSON equivalence outside the timer.
fn run_iteration(root: &Path, req: &AnalyzeRequest, repos: usize) -> Result<([Sample; 7], usize)> {
    let (cold, no_cache) = measure(|| run_analysis_with_options(req, true))?;
    let [finding] = cold.findings.as_slice() else {
        return Err("expected the generated response-field removal".into());
    };
    if !matches!(&finding.change, ContractChange::ResponseFieldRemoved { endpoint, field_path, .. }
        if endpoint.normalized_path == "/users" && field_path == "displayName")
        || !cold
            .coverage
            .as_ref()
            .is_some_and(|coverage| coverage.complete)
    {
        return Err("expected complete analysis of the generated display-name removal".into());
    }
    for index in 0..repos - 1 {
        let repo = format!("consumer-{index}");
        if !finding
            .consumers
            .iter()
            .any(|hit| hit.repo == repo && hit.confidence == Confidence::Confirmed)
        {
            return Err(format!("expected confirmed consumers in {repo}").into());
        }
    }
    let files_analyzed = cold.stats.files_analyzed;
    let baseline = root.join("baseline.json");
    fs::write(&baseline, render_json(&cold)?)?;
    drop(cold);
    let (populated, populate_cache) = measure(|| run_analysis_with_options(req, false))?;
    verify(&populated, &fs::read_to_string(&baseline)?, PHASES[1])?;
    drop(populated);
    let (warm, cache_hit) = measure(|| run_analysis_with_options(req, false))?;
    verify(&warm, &fs::read_to_string(&baseline)?, PHASES[2])?;
    drop(warm);
    let filled = index_workspace(&req.config_path)?;
    if (filled.cache_hits, filled.cache_misses) != (repos, 0) {
        return Err("analysis did not populate all repo cache entries".into());
    }

    let edited_repo = root.join("consumer-0");
    let mut edits = Vec::with_capacity(2);
    for (phase, file, original) in [
        (PHASES[3], "view-000.tsx", LEAF),
        (PHASES[4], "service.ts", SERVICE),
    ] {
        // Keep source locations stable while changing the committed blob.
        fs::write(
            edited_repo.join(file),
            format!("{original}// Benchmark edit\n"),
        )?;
        commit(&edited_repo, phase)?;
        let (mut incremental, sample) = measure(|| run_analysis_with_options(req, false))?;
        let uncached = run_analysis_with_options(req, true)?;
        verify(&incremental, &render_json(&uncached)?, phase)?;
        let baseline_json = fs::read_to_string(&baseline)?;
        normalize_edit(&mut incremental, &serde_json::from_str(&baseline_json)?)?;
        verify(&incremental, &baseline_json, phase)?;
        edits.push(sample);
    }

    fs::remove_file(root.join("index.sqlite3"))?;
    let (indexed, index_cold) = measure(|| index_workspace(&req.config_path))?;
    let (reindexed, index_warm) = measure(|| index_workspace(&req.config_path))?;
    if (
        indexed.repos_indexed,
        indexed.cache_hits,
        indexed.cache_misses,
    ) != (repos, 0, repos)
        || (
            reindexed.repos_indexed,
            reindexed.cache_hits,
            reindexed.cache_misses,
        ) != (repos, repos, 0)
    {
        return Err("index did not exercise the expected cold misses and warm hits".into());
    }
    Ok((
        [
            no_cache,
            populate_cache,
            cache_hit,
            edits[0],
            edits[1],
            index_cold,
            index_warm,
        ],
        files_analyzed,
    ))
}

#[derive(Serialize)]
struct Phase {
    phase: &'static str,
    median_ms: f64,
    min_ms: f64,
    max_ms: f64,
    peak_heap_mb: f64,
    os_rss_hwm_mb: Option<f64>,
    files_analyzed: Option<usize>,
    cache_speedup_x: f64,
}

// Summarize elapsed samples and report the maximum observed heap and RSS.
fn summarize(name: &'static str, samples: &[Sample], files_analyzed: Option<usize>) -> Phase {
    let mut times: Vec<_> = samples.iter().map(|sample| sample.ms).collect();
    times.sort_by(f64::total_cmp);
    let middle = times.len() / 2;
    let median_ms = if times.len().is_multiple_of(2) {
        f64::midpoint(times[middle - 1], times[middle])
    } else {
        times[middle]
    };
    Phase {
        phase: name,
        median_ms,
        min_ms: times[0],
        max_ms: times[times.len() - 1],
        peak_heap_mb: samples.iter().map(|s| s.peak_heap_mb).fold(0.0, f64::max),
        os_rss_hwm_mb: samples
            .iter()
            .filter_map(|s| s.os_rss_hwm_mb)
            .reduce(f64::max),
        files_analyzed,
        cache_speedup_x: 1.0,
    }
}

// Generate a fresh workspace per iteration and emit a single benchmark report.
fn main() -> Result<()> {
    let args = Args::parse();
    let (repos, files) = args.scale.size();
    let mut samples: [Vec<Sample>; 7] = Default::default();
    let mut files_analyzed = None;
    for _ in 0..args.iterations {
        let scratch = Scratch::new()?;
        let req = generate(&scratch.0, repos, files)?;
        let (iteration, analyzed) = run_iteration(&scratch.0, &req, repos)?;
        if files_analyzed
            .replace(analyzed)
            .is_some_and(|previous| previous != analyzed)
        {
            return Err("analysis coverage differed between iterations".into());
        }
        for (phase, sample) in samples.iter_mut().zip(iteration) {
            phase.push(sample);
        }
    }
    let mut phases: Vec<_> = PHASES
        .iter()
        .zip(&samples)
        .enumerate()
        .map(|(index, (&name, samples))| {
            summarize(name, samples, if index < 5 { files_analyzed } else { None })
        })
        .collect();
    let analysis_base = phases[0].median_ms;
    let index_base = phases[5].median_ms;
    for (index, phase) in phases.iter_mut().enumerate() {
        phase.cache_speedup_x =
            if index < 5 { analysis_base } else { index_base } / phase.median_ms;
    }
    if args.json {
        let output = serde_json::json!({
            "schema_version": 1,
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "scale": args.scale,
            "iterations": args.iterations,
            "repositories": repos,
            "tracked_files": files,
            "phases": phases,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!(
            "{} {}/{} | {} iterations | {repos} repos | {files} tracked files\n",
            args.scale.to_possible_value().unwrap().get_name(),
            std::env::consts::OS,
            std::env::consts::ARCH,
            args.iterations
        );
        println!(
            "| Phase | Median ms | Min ms | Max ms | Peak heap MB | OS RSS HWM MB | Files analyzed | Speedup |"
        );
        println!("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |");
        for phase in phases {
            let rss = phase
                .os_rss_hwm_mb
                .map_or_else(|| "—".into(), |mb| format!("{mb:.3}"));
            let analyzed = phase
                .files_analyzed
                .map_or_else(|| "—".into(), |count| count.to_string());
            println!(
                "| {} | {:.2} | {:.2} | {:.2} | {:.3} | {rss} | {analyzed} | {:.2}x |",
                phase.phase,
                phase.median_ms,
                phase.min_ms,
                phase.max_ms,
                phase.peak_heap_mb,
                phase.cache_speedup_x
            );
        }
    }
    Ok(())
}
