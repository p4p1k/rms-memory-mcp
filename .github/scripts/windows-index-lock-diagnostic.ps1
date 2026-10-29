$ErrorActionPreference = 'Stop'
$baseline = '239806054d7c00e515d5add30848dae01b1d1d89'
$module = 'crates/rms-memory-index/src/index_lock.rs'
git diff --exit-code $baseline HEAD -- $module
if ($LASTEXITCODE -ne 0) { throw 'Index lock differs from original v1.3.1' }
$source = (git show "${baseline}:$module") -join "`n"
if ($LASTEXITCODE -ne 0) { throw 'Original lock source unavailable' }
# This small diagnostic compiles the production module without LanceDB.
# The repository module and its existing tests remain unchanged.
$production = $source.Substring(0, $source.IndexOf('#[cfg(test)]'))
$guard = 'Err(error) if error.kind() == std::io::ErrorKind::WouldBlock'
if (($production.Split($guard).Count - 1) -ne 2) { throw 'Unexpected baseline lock implementation' }
$candidate = $production.Replace($guard, 'Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error()')
$root = Join-Path $env:RUNNER_TEMP 'rms-index-lock-diagnostic'
New-Item -ItemType Directory -Path "$root/src" | Out-Null
[IO.File]::WriteAllText("$root/src/original.rs", $production, [Text.UTF8Encoding]::new($false))
[IO.File]::WriteAllText("$root/src/candidate.rs", $candidate, [Text.UTF8Encoding]::new($false))
@'
[package]
name = "rms-index-lock-diagnostic"
version = "0.0.0"
edition = "2024"
[workspace]
[dependencies]
anyhow = "=1.0.103"
fs2 = "=0.4.3"
serde = { version = "=1.0.228", features = ["derive"] }
serde_json = "=1.0.150"
chrono = "=0.4.44"
tokio = { version = "=1.52.3", features = ["rt", "time"] }
'@ | Set-Content -Encoding utf8 "$root/Cargo.toml"
@'
#![allow(dead_code)]
mod original;
mod candidate;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;
    fn root(name: &str) -> String {
        let path = std::env::temp_dir().join(format!("rms-lock-probe-{}-{name}", std::process::id()));
        assert!(!path.exists(), "probe directory must be new");
        path.to_str().unwrap().to_owned()
    }
    #[test]
    fn original_v131_windows_contention_reports_error33_in_one_process() {
        let path = root("original");
        let held = original::try_acquire(&path).unwrap().expect("first lock");
        let error = match original::try_acquire(&path) {
            Err(error) => error,
            _ => panic!("expected original Windows contention defect"),
        };
        let io = error.downcast_ref::<std::io::Error>().unwrap();
        println!("DIAGNOSTIC pid={} file={}\\.index.lock operation=FileExt::try_lock_exclusive raw={:?} kind={:?} fs2_contention={:?}",
            std::process::id(), path, io.raw_os_error(), io.kind(), fs2::lock_contended_error().raw_os_error());
        assert_eq!(io.raw_os_error(), Some(33));
        assert_ne!(io.kind(), ErrorKind::WouldBlock);
        assert_eq!(io.raw_os_error(), fs2::lock_contended_error().raw_os_error());
        drop(held);
        assert!(original::try_acquire(&path).unwrap().is_some());
    }
    #[test]
    fn candidate_reports_busy_and_reacquires_after_release_without_retry() {
        let path = root("candidate");
        let held = candidate::try_acquire(&path).unwrap().expect("first lock");
        assert!(candidate::try_acquire(&path).unwrap().is_none());
        assert!(matches!(candidate::inspect(&path).unwrap(), candidate::LockInspection::Active(_)));
        drop(held);
        assert!(candidate::try_acquire(&path).unwrap().is_some());
        println!("PASS candidate busy -> release -> acquire, no retries or sleeps");
    }
    #[test]
    fn candidate_does_not_hide_unrelated_file_errors() {
        let path = root("file-error");
        std::fs::write(&path, "not a directory").unwrap();
        assert!(candidate::try_acquire(&path).is_err());
        println!("PASS unrelated filesystem errors still propagate");
    }
}
'@ | Set-Content -Encoding utf8 "$root/src/lib.rs"
rustup toolchain install 1.96.1 --profile minimal
if ($LASTEXITCODE -ne 0) { throw 'Runner Rust setup failed' }
cargo +1.96.1 test --manifest-path "$root/Cargo.toml" -- --nocapture --test-threads=1
if ($LASTEXITCODE -ne 0) { throw 'Windows lock diagnostics failed' }
'Windows index-lock diagnostics: original v1.3.1 contention and candidate regression checks PASS.' >> $env:GITHUB_STEP_SUMMARY
