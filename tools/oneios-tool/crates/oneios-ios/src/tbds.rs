//! `ios tbds`: .tbd stubs of the guest's own iOS 27 libraries, from the dyld shared cache of the image set (the
//! port of scripts/ios/make-tbds.py, retired 2026-11-21, which ran the `ipsw` program once per image). No outside
//! program: the cache is read by oneios-sym's reader (its files, mappings and images), each image's exports from
//! its export trie, and the cache's folder is copied out of systemos.img by image apfs-copy's reader when there is
//! no copy yet (no apfs-fuse, no hdiutil, no root).
//!
//! A stub has what the linker and macho-check need to bind to the library: its install name and current version,
//! its umbrella (LC_SUB_FRAMEWORK), the libraries it re-exports (LC_REEXPORT_DYLIB, whose own documents follow in
//! the same file, as Apple's SDK has them), and every name of its export trie: Objective-C classes (both the class
//! and the metaclass exported) and ivars in their own lists, weak definitions and thread-local variables in theirs,
//! the rest in `symbols`. What `ipsw dyld tbd` wrote beside that and the linker does not use, the classes and ivars
//! of the image's Objective-C metadata that it does not export, is not written (README "ios").

use crate::macho::{lc_str, u32_at, version_text, Segment, LC_DYLD_EXPORTS_TRIE, LC_DYLD_INFO, LC_DYLD_INFO_ONLY, LC_ID_DYLIB, LC_REEXPORT_DYLIB, LC_SEGMENT_64, LC_SUB_FRAMEWORK};
use crate::tbd::{self, Stub};
use anyhow::{anyhow, bail, Context, Result};
use oneios_json::Value;
use oneios_sym::Cache;
use rayon::prelude::*;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

pub const TARGET: &str = "arm64e-ios";

/// The stubs' target: arm64e-ios for an arm64e cache (iOS 27), arm64-ios for an arm64 one (iOS 16.5 of the iPhone X,
/// the Galaxy A56's image: `--cache out/ios16-cache --out out/ios16-tbd`, docs/a56-gpu.md).
pub fn target_of(main_name: &str) -> &'static str {
    if main_name.ends_with("arm64e") {
        TARGET
    } else {
        "arm64-ios"
    }
}
const CACHE_IN_VOLUME: &str = "System/Library/Caches/com.apple.dyld";

fn uleb(d: &[u8], at: &mut usize) -> Result<u64> {
    let (mut v, mut shift) = (0u64, 0u32);
    loop {
        let b = *d.get(*at).ok_or_else(|| anyhow!("the export trie is cut"))?;
        *at += 1;
        if shift < 64 {
            v |= u64::from(b & 0x7F) << shift;
        }
        if b & 0x80 == 0 {
            return Ok(v);
        }
        shift += 7;
        if shift > 70 {
            bail!("a ULEB128 of the export trie is too long");
        }
    }
}

/// One exported name: (name, flags). Flags: kind 1 thread-local, 2 absolute; 0x04 weak definition, 0x08
/// re-export, 0x10 stub and resolver.
pub fn trie_names(trie: &[u8]) -> Result<Vec<(String, u64)>> {
    let mut out = Vec::new();
    if trie.is_empty() {
        return Ok(out);
    }
    let mut stack: Vec<(usize, Vec<u8>)> = vec![(0, Vec::new())];
    let mut seen = std::collections::HashSet::new();
    while let Some((node, prefix)) = stack.pop() {
        if !seen.insert(node) {
            bail!("the export trie has a loop at {node:#x}");
        }
        let mut at = node;
        let size = uleb(trie, &mut at)? as usize;
        let children_at = at.checked_add(size).ok_or_else(|| anyhow!("the export trie is cut"))?;
        if size > 0 {
            let flags = uleb(trie, &mut at)?;
            out.push((String::from_utf8_lossy(&prefix).into_owned(), flags));
        }
        let mut at = children_at;
        let count = *trie.get(at).ok_or_else(|| anyhow!("the export trie is cut"))?;
        at += 1;
        for _ in 0..count {
            let end = trie.get(at..).and_then(|t| t.iter().position(|&b| b == 0)).ok_or_else(|| anyhow!("the export trie is cut"))?;
            let mut name = prefix.clone();
            name.extend_from_slice(&trie[at..at + end]);
            at += end + 1;
            let child = uleb(trie, &mut at)? as usize;
            if child >= trie.len() {
                bail!("an edge of the export trie points past its end");
            }
            stack.push((child, name));
        }
    }
    Ok(out)
}

/// The stub of one image of the cache.
pub fn image_stub(cache: &Cache, addr: u64, path: &str) -> Result<Stub> {
    let commands = cache.commands(addr)?;
    let mut stub = Stub { install_name: path.to_string(), ..Default::default() };
    let mut linkedit: Option<Segment> = None;
    let mut trie: Option<(u32, u32)> = None;
    for (cmd, b) in &commands {
        match *cmd {
            LC_ID_DYLIB => {
                stub.install_name = lc_str(b, 8);
                stub.current_version = u32_at(b, 16).map(version_text);
            }
            LC_SUB_FRAMEWORK => stub.umbrella = Some(lc_str(b, 8)),
            LC_REEXPORT_DYLIB => stub.reexported.push(lc_str(b, 8)),
            LC_SEGMENT_64 => {
                if let Some(s) = Segment::read(b).filter(|s| s.name == "__LINKEDIT") {
                    linkedit = Some(s);
                }
            }
            LC_DYLD_EXPORTS_TRIE => trie = Some((u32_at(b, 8).unwrap_or(0), u32_at(b, 12).unwrap_or(0))),
            LC_DYLD_INFO | LC_DYLD_INFO_ONLY if trie.is_none() => {
                let (off, size) = (u32_at(b, 40).unwrap_or(0), u32_at(b, 44).unwrap_or(0));
                if size > 0 {
                    trie = Some((off, size));
                }
            }
            _ => {}
        }
    }
    let names = match (trie, &linkedit) {
        (Some((off, size)), Some(le)) if size > 0 => {
            // the trie's offset is in the file that maps __LINKEDIT: its address is the segment's plus the distance
            let va = le.vmaddr.checked_add(u64::from(off)).and_then(|v| v.checked_sub(le.fileoff)).ok_or_else(|| anyhow!("{path}: the export trie is outside __LINKEDIT"))?;
            let data = cache.read(va, size as usize).ok_or_else(|| anyhow!("{path}: the export trie at {va:#x} is not in the cache"))?;
            if data.len() != size as usize {
                bail!("{path}: the export trie at {va:#x} is cut");
            }
            trie_names(data).with_context(|| path.to_string())?
        }
        (Some((_, size)), None) if size > 0 => bail!("{path}: an export trie but no __LINKEDIT"),
        _ => Vec::new(),
    };
    let all: BTreeSet<&str> = names.iter().map(|(n, _)| n.as_str()).collect();
    let mut classes = BTreeSet::new();
    for n in &all {
        if let Some(c) = n.strip_prefix("_OBJC_CLASS_$_") {
            if all.contains(format!("_OBJC_METACLASS_$_{c}").as_str()) {
                classes.insert(c.to_string());
            }
        }
    }
    let mut sorted = names;
    sorted.sort();
    sorted.dedup_by(|a, b| a.0 == b.0);
    for (name, flags) in sorted {
        if let Some(c) = name.strip_prefix("_OBJC_CLASS_$_").or_else(|| name.strip_prefix("_OBJC_METACLASS_$_")) {
            if classes.contains(c) {
                continue;
            }
        }
        if let Some(i) = name.strip_prefix("_OBJC_IVAR_$_") {
            stub.objc_ivars.push(i.to_string());
        } else if flags & 0x3 == 1 {
            stub.thread_local.push(name);
        } else if flags & 0x04 != 0 {
            stub.weak.push(name);
        } else {
            stub.symbols.push(name);
        }
    }
    stub.objc_classes = classes.into_iter().collect();
    Ok(stub)
}

/// make-tbds.py's `stub_path`: /usr/lib/libSystem.B.dylib -> usr/lib/libSystem.B.tbd, a framework's binary X ->
/// X.tbd.
pub fn stub_path(out: &Path, install_name: &str) -> PathBuf {
    let rel = install_name.trim_start_matches('/');
    let rel = rel.strip_suffix(".dylib").unwrap_or(rel);
    out.join(format!("{rel}.tbd"))
}

/// `-lSystem` looks for libSystem.tbd: a link beside each versioned stub of usr/lib (libSystem.B, libc++.1,
/// libz.1.2.x), as an SDK has.
pub fn aliases(out: &Path) -> Result<usize> {
    let lib = out.join("usr/lib");
    let Ok(dir) = std::fs::read_dir(&lib) else { return Ok(0) };
    let mut names: Vec<String> = dir.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    let re = regex::Regex::new(r"^(lib[^.]+(?:\+\+)?)((?:\.[A-Z0-9]+)+)\.tbd$").expect("the pattern");
    let mut n = 0;
    for name in names {
        if let Some(m) = re.captures(&name) {
            let link = lib.join(format!("{}.tbd", &m[1]));
            if std::fs::symlink_metadata(&link).is_err() {
                std::os::unix::fs::symlink(&name, &link).with_context(|| link.display().to_string())?;
                n += 1;
            }
        }
    }
    Ok(n)
}

fn count_stubs(dir: &Path) -> usize {
    let mut n = 0;
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    for e in rd.flatten() {
        let Ok(t) = e.file_type() else { continue };
        let name = e.file_name();
        if t.is_dir() {
            n += count_stubs(&e.path());
        } else if name.to_string_lossy().ends_with(".tbd") && (t.is_file() || e.path().is_file()) {
            n += 1;
        }
    }
    n
}

pub fn uuid_text(u: &[u8; 16]) -> String {
    let h: String = u.iter().map(|b| format!("{b:02X}")).collect();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

pub struct Options {
    pub cache: Option<PathBuf>,
    pub image: PathBuf,
    pub out: PathBuf,
    pub names: Vec<String>,
}

/// The folder of the cache: --cache (the folder or its main file), else out/ios27-cache, else a copy of the
/// image's cache folder made there.
fn cache_dir(repo: &Path, o: &Options) -> Result<(PathBuf, bool)> {
    if let Some(c) = &o.cache {
        let dir = if c.is_dir() { c.clone() } else { c.parent().map(Path::to_path_buf).unwrap_or_default() };
        let named = c.file_name().and_then(|n| n.to_str()).is_some_and(|n| oneios_sym::cache::MAIN_NAMES.contains(&n));
        if c.is_file() && !named {
            bail!("--cache {}: the main file is named {}", c.display(), oneios_sym::cache::MAIN_NAMES.join(" or "));
        }
        return Ok((dir, false));
    }
    let copy = repo.join("out/ios27-cache");
    if copy.join(oneios_sym::cache::MAIN_NAME).is_file() {
        return Ok((copy, false));
    }
    if !o.image.is_file() {
        bail!("no {} (build the image set first, or give --cache)", o.image.display());
    }
    eprintln!("tbds: copying /{CACHE_IN_VOLUME} of {} to {} (about 7 GB; kept for the next run)", o.image.display(), copy.display());
    let part = repo.join("out/ios27-cache.part");
    let _ = std::fs::remove_dir_all(&part);
    let image = o.image.to_string_lossy();
    let done = oneios_image::apfs_tools::copy_to(&image, CACHE_IN_VOLUME, &part.to_string_lossy(), None, oneios_image::apfs::Pick::Block0).map_err(|e| anyhow!("{image}: {e}"))?;
    std::fs::rename(&part, &copy)?;
    eprintln!("tbds: copied {} files, {} bytes", done.files, done.bytes);
    Ok((copy, true))
}

pub fn run(repo: &Path, o: &Options) -> Result<i32> {
    let (dir, copied) = cache_dir(repo, o)?;
    let cache = Cache::open(&dir).with_context(|| dir.display().to_string())?;
    let stub_target = target_of(cache.main_name);
    let images: Vec<(u64, String)> = cache.images.clone();
    let mut wanted: Vec<usize> = (0..images.len()).collect();
    if !o.names.is_empty() {
        wanted.clear();
        for want in &o.names {
            let hits: Vec<usize> = (0..images.len()).filter(|&i| images[i].1 == *want || Path::new(&images[i].1).file_name().is_some_and(|n| n == want.as_str())).collect();
            if hits.len() != 1 {
                bail!("{want}: {} images of the cache have that name", hits.len());
            }
            wanted.extend(hits);
        }
    }
    std::fs::create_dir_all(&o.out)?;
    // Every image's stub first: a file holds the documents of the libraries its image re-exports too.
    let done = AtomicUsize::new(0);
    let stubs: Vec<Result<Stub>> = images
        .par_iter()
        .map(|(addr, path)| {
            let r = image_stub(&cache, *addr, path);
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            if n % 1000 == 0 {
                eprintln!("tbds: read {n} of {} images", images.len());
            }
            r
        })
        .collect();
    let mut by_name: HashMap<&str, usize> = HashMap::new();
    for (i, s) in stubs.iter().enumerate() {
        by_name.insert(images[i].1.as_str(), i);
        if let Ok(s) = s {
            by_name.entry(s.install_name.as_str()).or_insert(i);
        }
    }
    let failed: Vec<String> = wanted
        .par_iter()
        .filter_map(|&i| {
            let target = stub_path(&o.out, &images[i].1);
            if target.exists() {
                return None;
            }
            let own = match &stubs[i] {
                Ok(s) => s,
                Err(e) => return Some(format!("{e:#}")),
            };
            let mut docs = vec![own];
            for lib in &own.reexported {
                match by_name.get(lib.as_str()).map(|&j| &stubs[j]) {
                    Some(Ok(s)) => docs.push(s),
                    Some(Err(e)) => return Some(format!("{}: re-exports {lib}: {e:#}", images[i].1)),
                    None => {}
                }
            }
            let text = tbd::write_file(&docs, stub_target);
            let write = || -> Result<()> {
                std::fs::create_dir_all(target.parent().unwrap_or(&o.out))?;
                let part = target.with_extension("tbd.part");
                std::fs::write(&part, text)?;
                std::fs::rename(&part, &target)?;
                Ok(())
            };
            write().err().map(|e| format!("{}: {e:#}", target.display()))
        })
        .collect();
    aliases(&o.out)?;
    let mut record = oneios_json::Map::new();
    let main = dir.join(cache.main_name);
    let shown = if copied { format!("{} (copied from {})", rel(repo, &main), rel(repo, &o.image)) } else { rel(repo, &main) };
    record.insert("cache".into(), Value::String(shown));
    record.insert("uuid".into(), Value::String(uuid_text(&cache.uuid)));
    record.insert("images_in_cache".into(), Value::Number(images.len().into()));
    let count = count_stubs(&o.out);
    record.insert("stubs".into(), Value::Number(count.into()));
    record.insert("writer".into(), Value::String(format!("oneios-tool ios tbds {}", env!("CARGO_PKG_VERSION"))));
    std::fs::write(o.out.join("tbd.json"), oneios_json::to_pretty(&Value::Object(record)) + "\n")?;
    for problem in failed.iter().take(20) {
        eprintln!("tbds: failed: {problem}");
    }
    println!("tbds: {count} stubs in {} ({} failed)", o.out.display(), failed.len());
    Ok(if failed.is_empty() { 0 } else { 1 })
}

fn rel(repo: &Path, p: &Path) -> String {
    p.strip_prefix(repo).map(|r| r.display().to_string()).unwrap_or_else(|_| p.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An arm64e cache (iOS 27) gives arm64e stubs, an arm64 one (iOS 16.5 of the iPhone X) arm64 stubs.
    #[test]
    fn the_target_follows_the_cache() {
        assert_eq!(target_of("dyld_shared_cache_arm64e"), "arm64e-ios");
        assert_eq!(target_of("dyld_shared_cache_arm64"), "arm64-ios");
        let dir = std::env::temp_dir().join(format!("tbds-target-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(oneios_sym::cache::main_name(&dir), "dyld_shared_cache_arm64e");
        std::fs::write(dir.join("dyld_shared_cache_arm64"), b"").unwrap();
        assert_eq!(oneios_sym::cache::main_name(&dir), "dyld_shared_cache_arm64");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A trie of `_a` (regular), `_ab` (weak), `_t` (thread-local) and `_r` (a re-export).
    fn trie() -> Vec<u8> {
        // root: no terminal; one child "_" -> node1
        let mut t = vec![0x00, 0x01, b'_', 0x00, 0x05];
        // node1 at 5: no terminal; children "a" -> n_a, "t" -> n_t, "r" -> n_r
        assert_eq!(t.len(), 5);
        t.extend([0x00, 0x03, b'a', 0x00, 0, b't', 0x00, 0, b'r', 0x00, 0]);
        let n_a = t.len();
        // n_a: terminal (flags 0, address 0x10); child "b" -> n_ab
        t.extend([0x02, 0x00, 0x10, 0x01, b'b', 0x00, 0]);
        let fix_ab = t.len() - 1;
        let n_ab = t.len();
        t.extend([0x02, 0x04, 0x20, 0x00]);
        let n_t = t.len();
        t.extend([0x02, 0x01, 0x30, 0x00]);
        let n_r = t.len();
        // re-export: flags 8, ordinal 1, name "_other"
        t.extend([0x09, 0x08, 0x01, b'_', b'o', b't', b'h', b'e', b'r', 0x00, 0x00]);
        t[9] = n_a as u8;
        t[12] = n_t as u8;
        t[15] = n_r as u8;
        t[fix_ab] = n_ab as u8;
        t
    }

    #[test]
    fn reads_an_export_trie() {
        let mut got = trie_names(&trie()).unwrap();
        got.sort();
        assert_eq!(got, vec![("_a".into(), 0), ("_ab".into(), 4), ("_r".into(), 8), ("_t".into(), 1)]);
        assert!(trie_names(&[]).unwrap().is_empty());
        // a cut trie and a loop are errors
        assert!(trie_names(&trie()[..20]).is_err());
        assert!(trie_names(&[0x00, 0x01, b'x', 0x00, 0x00]).is_err());
    }

    #[test]
    fn paths_and_aliases() {
        let out = Path::new("/o");
        assert_eq!(stub_path(out, "/usr/lib/libSystem.B.dylib"), PathBuf::from("/o/usr/lib/libSystem.B.tbd"));
        assert_eq!(stub_path(out, "/System/Library/Frameworks/Metal.framework/Metal"), PathBuf::from("/o/System/Library/Frameworks/Metal.framework/Metal.tbd"));
        let dir = std::env::temp_dir().join(format!("oneios-ios-aliases-{}", std::process::id()));
        let lib = dir.join("usr/lib");
        std::fs::create_dir_all(&lib).unwrap();
        for n in ["libSystem.B.tbd", "libc++.1.tbd", "libz.1.2.12.tbd", "libobjc.A.tbd", "libplain.tbd", "libicucore.A.tbd", "libicucore.tbd"] {
            std::fs::write(lib.join(n), "").unwrap();
        }
        assert_eq!(aliases(&dir).unwrap(), 4);
        for (link, to) in [("libSystem.tbd", "libSystem.B.tbd"), ("libc++.tbd", "libc++.1.tbd"), ("libz.tbd", "libz.1.2.12.tbd"), ("libobjc.tbd", "libobjc.A.tbd")] {
            assert_eq!(std::fs::read_link(lib.join(link)).unwrap(), PathBuf::from(to));
        }
        assert!(std::fs::symlink_metadata(lib.join("libicucore.tbd")).unwrap().file_type().is_file());
        assert_eq!(count_stubs(&dir), 11);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn uuid_as_ipsw_wrote_it() {
        let u = [0x77, 0xBC, 0x8C, 0x63, 0x93, 0xDA, 0x3E, 0x9B, 0xBB, 0x9C, 0x0B, 0xE2, 0xAF, 0x0C, 0x0E, 0x17];
        assert_eq!(uuid_text(&u), "77BC8C63-93DA-3E9B-BB9C-0BE2AF0C0E17");
    }
}
