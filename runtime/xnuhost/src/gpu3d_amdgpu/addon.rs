// SPDX-License-Identifier: MIT
//! Whether the kernel's gpuaddon sidecar (<kernel>.gpuaddon: oneios-tool's NGPUADD2, tools/oneios-tool/crates/
//! oneios-ios/src/metal.rs `pack`) carries the A56's Metal plug-in (K3): `addons/OneiOSRadeonMetal.bundle/` with its
//! executable `OneiOSRadeonMetal` and its `oneios-target`, the line userland/metal/build.sh's `finish` writes
//! ("MODEL BUILD", here `iPhone10,3 20F66`). Without the plug-in the guest has no Metal on this GPU, so xnuhost keeps
//! the 2D path and says why.
use xh_core::bytes::le;

/// The plug-in's name (its bundle, its executable).
pub const PLUGIN: &str = "OneiOSRadeonMetal";

/// What the sidecar holds of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plugin {
    /// its oneios-target line
    pub target: String,
    /// files under its bundle
    pub files: usize,
}

/// A file of the sidecar: its name, its packed bytes, its unpacked size.
type File<'a> = (&'a [u8], &'a [u8], u64);

/// The sidecar's files; Err when it is not NGPUADD2 or is cut.
fn files(blob: &[u8]) -> Result<Vec<File<'_>>, String> {
    let cut = || "the gpuaddon is cut short".to_owned();
    if blob.get(..8) != Some(b"NGPUADD2".as_slice()) {
        return Err("the gpuaddon is not NGPUADD2".into());
    }
    let count = le::try_u32(blob, 8).ok_or_else(cut)? as usize;
    let mut at = 24;
    let mut out = Vec::new();
    for _ in 0..count {
        let nlen = le::try_u32(blob, at).ok_or_else(cut)? as usize;
        let size = le::try_u64(blob, at + 8).ok_or_else(cut)?;
        let plen = le::try_u64(blob, at + 16).ok_or_else(cut)? as usize;
        at += 24;
        if nlen == 0 {
            return Err(cut());
        }
        let name = blob.get(at..at + nlen - 1).ok_or_else(cut)?;
        at += nlen.div_ceil(8) * 8;
        let packed = blob.get(at..at.checked_add(plen).ok_or_else(cut)?).ok_or_else(cut)?;
        at += plen.div_ceil(8) * 8;
        out.push((name, packed, size));
    }
    Ok(out)
}

/// The plug-in in `addon` (None: there is no sidecar), for an image set of build `build` (None: no image set):
/// Ok(what it holds), or Err(why not) in the words of the session's "gpu: 2D only" line.
pub fn plugin_in_addon(addon: Option<&[u8]>, build: Option<&str>) -> Result<Plugin, String> {
    let Some(blob) = addon else {
        return Err(format!("the kernel has no gpuaddon sidecar, so no {PLUGIN} plug-in (the A56's Metal)"));
    };
    let list = files(blob)?;
    let dir = format!("addons/{PLUGIN}.bundle/");
    let exe = format!("{dir}{PLUGIN}");
    let target = format!("{dir}oneios-target");
    let mine: Vec<_> = list.iter().filter(|(n, _, _)| n.starts_with(dir.as_bytes())).collect();
    if !mine.iter().any(|(n, _, _)| *n == exe.as_bytes()) {
        return Err(format!("the gpuaddon has no {PLUGIN} plug-in (the A56's Metal)"));
    }
    let Some((_, packed, size)) = mine.iter().find(|(n, _, _)| *n == target.as_bytes()) else {
        return Err(format!("the {PLUGIN} plug-in in the gpuaddon has no oneios-target"));
    };
    let text = xh_core::inflate::zlib(packed, 256)
        .ok()
        .filter(|t| t.len() as u64 == *size)
        .ok_or_else(|| format!("the {PLUGIN} plug-in's oneios-target does not unpack"))?;
    let line = String::from_utf8_lossy(&text).trim().to_owned();
    let named = line.split_whitespace().nth(1).unwrap_or("");
    if let Some(build) = build.filter(|b| *b != named) {
        return Err(format!("the {PLUGIN} plug-in is for \"{line}\", the image set is build {build}"));
    }
    Ok(Plugin { target: line, files: mine.len() })
}
