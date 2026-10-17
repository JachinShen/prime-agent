//! The runtime-ready probe concern: the two-layer memo (in-process map + the cross-process on-disk
//! verdict) that skips re-probing a venv nobody damaged.

use super::{
    collect_python_files, Digest, HashMap, KernelPythonSkill, Mutex, Path, PathBuf, Stdio,
};

fn python_imports(python: &str, module_name: &str) -> bool {
    run_quiet(python, &["-c", &format!("import {module_name}")])
}

fn run_quiet(command: &str, args: &[&str]) -> bool {
    let mut child = std::process::Command::new(command);
    child
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Hidden window on Windows (TS `spawnHidden`).
    crate::platform::process::set_no_window(&mut child);
    if cfg!(windows) {
        child.env("PYTHONUTF8", "1");
    }
    matches!(child.status(), Ok(status) if status.success())
}

/// The Nix adapter transport entry point and wire protocol must be available.
const RUNTIME_READY_CHECK: &str =
    "import rlm.nix_repl as repl; assert callable(repl.main); assert repl.PROTOCOL_VERSION == 3";

pub(crate) fn has_prime_agent_runtime(python: &str) -> bool {
    run_quiet(python, &["-c", RUNTIME_READY_CHECK])
}

pub(crate) fn missing_python_skill_import_labels(
    python: &str,
    python_skills: &[KernelPythonSkill],
) -> Vec<String> {
    python_skills
        .iter()
        .filter(|skill| !python_imports(python, &skill.import_name))
        .map(|skill| format!("{} ({})", skill.name, skill.import_name))
        .collect()
}

/// Process-global memo of a successful runtime-ready probe, tiered above the cross-process on-disk
/// memo ([`super::super::disk_memo`]): the probe is a full interpreter start. Memoized on success
/// only: the key carries every input the probe observes; a failed kernel start drops both layers
/// ([`invalidate_runtime_probe_cache`]).
static RUNTIME_PROBE_MEMO: Mutex<Option<HashMap<String, PathBuf>>> = Mutex::new(None);

pub(super) fn lock_probe_memo() -> std::sync::MutexGuard<'static, Option<HashMap<String, PathBuf>>>
{
    RUNTIME_PROBE_MEMO
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Identity of the runtime as installed in the venv: the interpreter binary's stat plus a content
/// hash of the installed `rlm` package tree. Out-of-band damage changes this identity, so a
/// memoized probe result can never mask a mutated install.
pub(super) fn installed_runtime_identity(python: &Path, venv: &Path) -> String {
    let mut hasher = sha2::Sha256::new();
    match std::fs::metadata(python) {
        Ok(meta) => {
            let modified = meta
                .modified()
                .map_or_else(|_| "no-mtime".to_string(), |time| format!("{time:?}"));
            hasher.update(format!("py:{}:{}:{modified}", python.display(), meta.len()).as_bytes());
        }
        Err(error) => hasher.update(format!("py-error:{}:{error}", python.display()).as_bytes()),
    }
    match installed_package_dir(venv, "rlm") {
        Some(dir) => match hash_python_tree(&dir) {
            Ok(hash) => hasher.update(format!("rlm:{hash}").as_bytes()),
            Err(error) => hasher.update(format!("rlm-error:{error}").as_bytes()),
        },
        None => hasher.update(b"rlm-missing"),
    }
    format!("sha256:{:x}", hasher.finalize())
}

/// The installed `rlm` package under the venv's site-packages: the Windows layout
/// `<venv>/Lib/site-packages/rlm` (no python-version layer) or the Unix layout
/// `<venv>/lib/python*/site-packages/rlm`.
#[cfg(test)]
pub(super) fn installed_rlm_dir(venv: &Path) -> Option<PathBuf> {
    installed_package_dir(venv, "rlm")
}

pub(super) fn installed_package_dir(venv: &Path, package: &str) -> Option<PathBuf> {
    let lib = venv.join("lib");
    let windows_layout = lib.join("site-packages").join(package);
    if windows_layout.is_dir() {
        return Some(windows_layout);
    }
    let entries = std::fs::read_dir(&lib).ok()?;
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        if !entry.file_name().to_string_lossy().starts_with("python") {
            continue;
        }
        let installed = entry.path().join("site-packages").join(package);
        if installed.is_dir() {
            return Some(installed);
        }
    }
    None
}

/// Content hash of a python tree: every `.py` file's relative path and
/// bytes, in sorted order (same witness shape as `hash_runtime_source`).
fn hash_python_tree(dir: &Path) -> anyhow::Result<String> {
    let mut files = Vec::new();
    collect_python_files(dir, &mut files)?;
    files.sort();
    let mut hasher = sha2::Sha256::new();
    for file in &files {
        let relative = file.strip_prefix(dir)?;
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update([0]);
        hasher.update(&std::fs::read(file)?);
        hasher.update([0]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// The memo key: every input the runtime-ready probe observes.
pub(super) fn runtime_probe_key(
    python: &str,
    runtime_identity: &str,
    version_raw: &str,
    installed_identity: &str,
) -> String {
    format!(
        "{python}\u{0}{runtime_identity}\u{0}{installed_identity}\u{0}sha256:{:x}",
        sha2::Sha256::digest(version_raw.as_bytes())
    )
}

/// The runtime-ready check, memoized on success across two layers: the process-global map first,
/// then the on-disk cross-process memo. `version_raw` is the raw `.bootstrap-version` text the
/// caller already read. The key is recomputed fresh on every call so a hit skips only the
/// interpreter probe. A caller-owned `PRIME_AGENT_KERNEL_PYTHON` override never reaches this.
pub(super) fn has_prime_agent_runtime_memoized(
    python: &str,
    runtime_identity: &str,
    version_raw: &str,
    installed_identity: &str,
    venv: &Path,
) -> bool {
    let key = runtime_probe_key(python, runtime_identity, version_raw, installed_identity);
    let memo_path = super::super::disk_memo::disk_memo_path(venv);
    if lock_probe_memo()
        .as_ref()
        .is_some_and(|memo| memo.contains_key(&key))
    {
        return true;
    }
    if super::super::disk_memo::disk_memo_hit(&memo_path, &key) {
        let mut memo = lock_probe_memo();
        let entries = memo.get_or_insert_with(HashMap::new);
        if entries.len() >= 16 {
            entries.clear();
        }
        entries.insert(key, memo_path);
        return true;
    }
    if !has_prime_agent_runtime(python) {
        return false;
    }
    let mut memo = lock_probe_memo();
    let entries = memo.get_or_insert_with(HashMap::new);
    if entries.len() >= 16 {
        entries.clear();
    }
    super::super::disk_memo::disk_memo_write(&memo_path, &key);
    entries.insert(key, memo_path);
    true
}

/// Drop every memoized runtime-ready result, both layers: the in-process map dies with this call.
/// The next kernel start re-runs the probe.
pub fn invalidate_runtime_probe_cache() {
    let tracked: Vec<PathBuf> = lock_probe_memo()
        .take()
        .map(|memo| memo.values().cloned().collect())
        .unwrap_or_default();
    for path in tracked {
        super::super::disk_memo::disk_memo_invalidate(&path);
    }
}

/// Drop only the in-process memo layer, leaving the on-disk layer intact:
/// the fresh-process simulation the disk-memo oracles use. Unix only.
#[cfg(all(test, unix))]
pub(crate) fn clear_in_process_probe_memo_for_tests() {
    *lock_probe_memo() = None;
}
