// SPDX-License-Identifier: GPL-3.0-or-later
//
// priel — hi-res terminal client for TIDAL
// Copyright (C) 2026 Guy Boldon
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

//! An on-disk cache for fetched assets, starting with album art.
//!
//! Album art is immutable per URL - a cover id names one image forever - so a
//! cached cover is always valid and never needs revalidating. That makes it the
//! clean first thing to cache: reopening a listing re-shows covers rather than
//! re-downloading them. (Listing *metadata* can change on another device and so
//! needs a freshness policy; that is a later stage. Audio is never cached - a
//! subscription licenses streaming, not a stored copy.)
//!
//! The cache lives in the XDG cache directory, which is exactly where
//! regenerable, machine-local data belongs - not config (it is not a setting)
//! and not state (losing it costs nothing but a re-download). It is bounded: a
//! `put` evicts the least-recently-used files once the directory passes the cap,
//! so the cache cannot grow without limit.
//!
//! Every path is passed in, never resolved from a global inside the read/write,
//! so a test writes to a directory it made and never to a real home - the rule
//! the whole workspace keeps.

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The default cache ceiling: 256 MiB, in bytes.
///
/// Art is small (a 160 px cover is tens of kilobytes), so this holds a large
/// library's covers many times over; the cap is a backstop, not a squeeze.
pub const DEFAULT_CAP_BYTES: u64 = DEFAULT_CAP_MIB * 1024 * 1024;

/// The default cache ceiling in whole MiB - the unit the flag and settings key
/// speak, so the default reads the same in both places. Defined in `cli`, the
/// one module the asset generator also compiles, so a single figure serves the
/// flag, the settings default and the cache.
pub const DEFAULT_CAP_MIB: u64 = crate::cli::DEFAULT_CACHE_MIB;

/// The album-art cache directory: `$XDG_CACHE_HOME/priel/covers`, or
/// `~/.cache/priel/covers` when `XDG_CACHE_HOME` is unset or empty.
///
/// `None` when neither is known, which is the one machine priel cannot cache on
/// - and the caller treats that as "just fetch", never an error.
#[must_use]
pub fn cover_dir() -> Option<PathBuf> {
    cache_subdir(env::var_os("XDG_CACHE_HOME"), env::var_os("HOME"), "covers")
}

/// The listing-metadata cache directory: `$XDG_CACHE_HOME/priel/meta`, or
/// `~/.cache/priel/meta`.
///
/// Separate from `covers` so eviction over one does not reach into the other,
/// and so the two are easy to tell apart on disk. Unlike a cover, cached
/// metadata can go stale (a listing changes on another device), so a caller
/// serves it only while revalidating it. `None` when no cache directory is
/// known - the caller then just fetches, which is never an error.
#[must_use]
pub fn meta_dir() -> Option<PathBuf> {
    cache_subdir(env::var_os("XDG_CACHE_HOME"), env::var_os("HOME"), "meta")
}

/// The `priel/<sub>` cache directory under `$XDG_CACHE_HOME`, or `~/.cache` when
/// that is unset or empty. Its inputs are passed so the resolution is a table of
/// tests rather than something that reads the real environment.
fn cache_subdir(xdg: Option<OsString>, home: Option<OsString>, sub: &str) -> Option<PathBuf> {
    let base = xdg
        .filter(|x| !x.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("priel").join(sub))
}

/// A key as a safe filename: a cover id can carry a slash or other path-unsafe
/// character, so anything that is not a plain identifier becomes an underscore.
fn key_file(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The cached bytes for `key`, if present, refreshing its recency so a cover
/// still being looked at survives eviction.
///
/// The touch is best-effort: a filesystem that will not let priel set the time
/// only means this read does not count towards keeping the file, never a
/// failure to serve it.
#[must_use]
pub fn get(dir: &Path, key: &str) -> Option<Vec<u8>> {
    let path = dir.join(key_file(key));
    let bytes = std::fs::read(&path).ok()?;
    let _ = touch(&path);
    Some(bytes)
}

/// Store `bytes` under `key`, then evict the least-recently-used files while the
/// directory is over `cap_bytes`.
///
/// # Errors
///
/// The `std::io::Error` from creating the directory or writing the file. A
/// failed eviction is swallowed - the cache being briefly over its cap is not
/// worth failing a write nobody asked to fail.
pub fn put(dir: &Path, key: &str, bytes: &[u8], cap_bytes: u64) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join(key_file(key)), bytes)?;
    evict(dir, cap_bytes);
    Ok(())
}

/// Set a file's modified time to now, so a read counts as recent use.
fn touch(path: &Path) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .set_modified(SystemTime::now())
}

/// Delete least-recently-used files until the directory is within `cap_bytes`.
///
/// Recency is the modified time, which [`get`] refreshes on a read and [`put`]
/// sets on a write - so the file just written is the newest and the last thing
/// eviction would ever take. Best-effort throughout: a directory that cannot be
/// read, or a file that will not delete, leaves the cache a little over rather
/// than erroring.
fn evict(dir: &Path, cap_bytes: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(PathBuf, u64, SystemTime)> = entries
        .flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            Some((e.path(), meta.len(), meta.modified().ok()?))
        })
        .collect();
    let mut total: u64 = files.iter().map(|(_, len, _)| *len).sum();
    if total <= cap_bytes {
        return;
    }
    // Oldest first: the least recently used are the first to go.
    files.sort_by_key(|(_, _, modified)| *modified);
    for (path, len, _) in files {
        if total <= cap_bytes {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_CAP_BYTES, cache_subdir, evict, get, key_file, put};
    use std::ffi::OsString;
    use std::path::PathBuf;

    /// A fresh empty directory under the OS temp dir, unique to this process and
    /// test name - never a real cache. Removed first so a rerun starts clean.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("priel-cache-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn the_cache_dirs_prefer_xdg_cache_then_fall_back_to_home() {
        // Goal: regenerable, machine-local data belongs in the XDG cache dir, and
        // covers and metadata sit in sibling subdirectories under it.
        assert_eq!(
            cache_subdir(Some("/x/cache".into()), Some("/home/u".into()), "covers"),
            Some(PathBuf::from("/x/cache/priel/covers"))
        );
        assert_eq!(
            cache_subdir(None, Some("/home/u".into()), "meta"),
            Some(PathBuf::from("/home/u/.cache/priel/meta"))
        );
        assert_eq!(
            cache_subdir(Some(OsString::new()), Some("/home/u".into()), "covers"),
            Some(PathBuf::from("/home/u/.cache/priel/covers")),
            "an empty XDG_CACHE_HOME is unset"
        );
        assert_eq!(
            cache_subdir(None, None, "meta"),
            None,
            "no home, nowhere to cache"
        );
    }

    #[test]
    fn a_metadata_blob_reads_back_from_its_own_dir() {
        // Goal: the metadata cache reuses the same get/put over its own
        // directory, so a serialized listing round-trips exactly as a cover does.
        let dir = scratch("meta-roundtrip");
        assert!(get(&dir, "favorites").is_none(), "nothing cached yet");
        put(
            &dir,
            "favorites",
            br#"{"items":[],"total":9}"#,
            DEFAULT_CAP_BYTES,
        )
        .expect("put");
        assert_eq!(
            get(&dir, "favorites").as_deref(),
            Some(&br#"{"items":[],"total":9}"#[..])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stored_cover_reads_back_and_a_missing_one_is_none() {
        // Goal: the whole point - a put then a get returns the same bytes, and a
        // key never stored is a miss (the caller then fetches).
        let dir = scratch("roundtrip");
        assert!(get(&dir, "cover1").is_none(), "nothing stored yet");
        put(&dir, "cover1", b"the-jpeg-bytes", DEFAULT_CAP_BYTES).expect("put");
        assert_eq!(get(&dir, "cover1").as_deref(), Some(&b"the-jpeg-bytes"[..]));
        assert!(
            get(&dir, "cover2").is_none(),
            "a different key still misses"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_put_over_the_cap_evicts_the_oldest_file() {
        // Goal: the cache is bounded. With a cap of two 100-byte covers, adding a
        // third evicts the least-recently-used (oldest-touched) one.
        let dir = scratch("evict");
        let big = vec![b'x'; 100];
        // Written oldest to newest; sleep is not available, so stamp the mtimes
        // directly so the ordering is deterministic rather than timing-dependent.
        for (i, key) in ["a", "b", "c"].iter().enumerate() {
            put(&dir, key, &big, 250).expect("put");
            let mtime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1000 + i as u64);
            std::fs::OpenOptions::new()
                .write(true)
                .open(dir.join(key))
                .expect("open")
                .set_modified(mtime)
                .expect("set mtime");
        }
        // Three 100-byte files = 300 > 250; evict brings it under.
        evict(&dir, 250);
        assert!(!dir.join("a").exists(), "the oldest was evicted");
        assert!(dir.join("b").exists(), "the newer two survive");
        assert!(dir.join("c").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_key_with_path_characters_is_made_a_safe_filename() {
        // Goal: a cover id can carry a slash; the cache must not write outside its
        // own directory or fail to store it.
        assert_eq!(key_file("ab/cd-12_3"), "ab_cd-12_3");
        let dir = scratch("safekey");
        put(&dir, "a/b/c", b"z", DEFAULT_CAP_BYTES).expect("put");
        assert_eq!(get(&dir, "a/b/c").as_deref(), Some(&b"z"[..]));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
