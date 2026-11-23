// SPDX-License-Identifier: MIT
// tests/test_imageset.c: the manifest reader, chunk verification, the trust cache sidecar from IM4P files, the device
// tree properties, on generated stand-in files (no Apple files). With ONEIOS_IMAGE_SET=DIR it also reads a real set's
// manifest, hash lists and trust caches.

use super::*;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

const ABC: [u8; 32] = [
    0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae, 0x22, 0x23, 0xb0, 0x03, 0x61,
    0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61, 0xf2, 0x00, 0x15, 0xad,
];

#[test]
fn sha256_vectors() {
    assert_eq!(sha256(b"abc"), ABC);
    assert_eq!(hex(&sha256(b"")), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    // the same in pieces, across the block boundary
    let million = vec![b'a'; 1_000_000];
    let mut s = Sha256::new();
    for piece in million.chunks(977) {
        s.update(piece);
    }
    let d = s.finalize();
    assert!(d[0] == 0xcd && d[1] == 0xc7 && d[2] == 0x6e && d[31] == 0xd0);
    // every padding length (55, 56, 63, 64 bytes are the edges), the portable path against the one in use
    for n in 0..200 {
        let data: Vec<u8> = (0..n as u32).map(|i| (i * 7) as u8).collect();
        let mut h = Sha256::new();
        h.update(&data);
        let mut portable = Sha256::new();
        let mut blocks = data.chunks_exact(64);
        for b in &mut blocks {
            block_portable(&mut portable.h, b.try_into().unwrap());
        }
        portable.bytes = n as u64 - blocks.remainder().len() as u64;
        portable.update(blocks.remainder());
        assert_eq!(h.finalize(), portable.finalize(), "{n} bytes");
    }
}

/// An IM4P around a version 2 trust cache module with `count` entries whose first byte is `first + i`.
fn im4p(kind: &[u8; 4], count: usize, first: u8) -> Vec<u8> {
    let payload = 24 + count * 24;
    let body = 6 + 6 + 3 + 4 + payload;
    let mut p = vec![0x30, 0x82, (body >> 8) as u8, body as u8, 0x16, 4];
    p.extend_from_slice(b"IM4P");
    p.extend_from_slice(&[0x16, 4]);
    p.extend_from_slice(kind);
    p.extend_from_slice(&[0x16, 1, b'1', 0x04, 0x82, (payload >> 8) as u8, payload as u8]);
    let mut m = vec![0u8; payload];
    m[0] = 2;
    m[4..20].fill(0xab);
    m[20] = count as u8;
    for i in 0..count {
        m[24 + i * 24] = first + i as u8;
        m[24 + i * 24 + 20] = 2;
    }
    p.extend_from_slice(&m);
    p
}

struct Rig {
    dir: PathBuf,
    entries: Vec<String>,
}

impl Rig {
    fn put(&self, name: &str, data: &[u8]) {
        fs::write(self.dir.join(name), data).expect("test file");
    }
    fn entry(&mut self, path: &str, data: &[u8], more: &str) {
        self.put(path, data);
        self.entries.push(format!(
            "\n    {{\"path\": \"{path}\", \"size\": {}, \"sha256\": \"{}\"{more}}}",
            data.len(),
            hex(&sha256(data))
        ));
    }
}

#[test]
fn image_set() {
    let dir = std::env::temp_dir().join(format!("test-imageset-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    for sub in ["trust", "overlay/userland/bin"] {
        fs::create_dir_all(dir.join(sub)).unwrap();
    }
    let dirs = dir.to_str().unwrap().to_owned();
    let mut rig = Rig { dir: dir.clone(), entries: Vec::new() };

    // An image of two and a half chunks, and its hash list.
    const CHUNK: usize = 1 << 20;
    const IMAGE: usize = CHUNK * 2 + CHUNK / 2;
    let image: Vec<u8> = (0..IMAGE).map(|i| ((i as u32).wrapping_mul(2654435761) >> 24) as u8).collect();
    let mut list = vec![0u8; 32 + 3 * 32];
    list[..5].copy_from_slice(b"OIHL1");
    list[10] = 0x10; // 1 MiB
    list[12] = 1;
    list[18] = 0x28; // 0x280000
    list[24] = 3;
    for c in 0..3 {
        let end = if c == 2 { c * CHUNK + CHUNK / 2 } else { (c + 1) * CHUNK };
        list[32 + c * 32..64 + c * 32].copy_from_slice(&sha256(&image[c * CHUNK..end]));
    }
    let mut trust = [im4p(b"trst", 3, 0x10), im4p(b"trcs", 2, 0x40), im4p(b"trca", 1, 0x80)];
    let helper = [0x5au8; 700];
    let owner = "{\"schema\": 1, \"cdhashes\": [\"ff000000000000000000000000000000000000aa\", \"0100000000000000000000000000000000000001\", \"ff000000000000000000000000000000000000aa\"], \"files\": []}";
    rig.entry("data.seed", b"seed", ", \"role\": \"data-seed\", \"format\": \"tar\"");
    rig.entry("os.img", &image, ", \"role\": \"os\", \"hashes\": \"os.img.hashes\", \"apfs\": {\"container_offset\": 0, \"volumes\": [{\"name\": \"OS\", \"uuid\": \"BBA627F7-F5DC-47E9-BD09-F4186021465B\", \"sealed\": false}]}");
    rig.entry("os.img.hashes", &list, ", \"role\": \"hashes\"");
    rig.entry("overlay/userland/bin/oneios-seed", &helper, ", \"role\": \"overlay\"");
    rig.entry("overlay/userland/owner-trust.json", owner.as_bytes(), ", \"role\": \"overlay\"");
    rig.entry("trust/app.trustcache", &trust[2], ", \"role\": \"trust-app\"");
    rig.entry("trust/static.trustcache", &trust[0], ", \"role\": \"trust-static\"");
    rig.entry("trust/system.trustcache", &trust[1], ", \"role\": \"trust-system\"");
    let manifest = format!(
        "{{\n  \"format\": 1, \"name\": \"oneios-image\", \"build\": \"24A446\",\n  \"data\": {{\"seed\": \"data.seed\", \"volume_uuid\": \"1B7DB2B7-5ECA-4FDA-98AF-55DF8632E355\", \"volume_name\": \"Data\", \"case_sensitive\": true, \"store_gib\": 16}},\n  \"tweaks\": {{\"info\": {{\"nested\": [1, {{\"x\": \"a\\\"b\"}}]}}}},\n  \"files\": [{}\n  ]\n}}\n",
        rig.entries.join(",")
    );
    rig.put("manifest.json", manifest.as_bytes());

    let set = ImageSet::load(&dirs).unwrap().unwrap();
    assert!(set.files.len() == 8 && set.build == "24A446" && set.store_gib == 16 && set.data_name == "Data");
    assert_eq!(set.data_uuid, "1B7DB2B7-5ECA-4FDA-98AF-55DF8632E355");
    let path = format!("{dirs}/os.img");
    let os = set.find(&path).unwrap();
    assert!(std::ptr::eq(os, set.role("os").unwrap()) && os.size == IMAGE as u64);
    assert_eq!(os.volume_uuid, "BBA627F7-F5DC-47E9-BD09-F4186021465B");
    assert!(set.find("nothing.img").is_none() && set.role("data-seed").is_some());
    assert!(set.find("os.img").is_some()); // the manifest's own path too

    // chunk verification: the image as written, then with one byte of the last chunk changed
    let v = Verify::open(&set, os).unwrap();
    assert_eq!(v.chunks(), 3);
    let file = OpenOptions::new().read(true).write(true).open(&path).unwrap();
    let (c, i) = (CHUNK as u64, IMAGE as u64);
    assert!(v.range(&file, 0, 512).is_ok());
    assert!(v.range(&file, c - 512, 1024).is_ok()); // across two chunks
    assert!(v.range(&file, i - 512, 1024).is_err()); // past the end
    assert!(v.range(&file, i, 0).is_ok());
    file.write_all_at(&[!image[IMAGE - 1]], i - 1).unwrap();
    assert!(v.range(&file, 2 * c, 512).is_err()); // the changed chunk is refused
    assert!(v.range(&file, 0, 2 * c).is_ok()); // the others still pass
    file.write_all_at(&[image[IMAGE - 1]], i - 1).unwrap();
    assert!(v.range(&file, 2 * c, c / 2).is_ok());
    assert_eq!(v.digest(), &set.find("os.img.hashes").unwrap().sha256);
    // several threads on one image at once: each chunk checked, the bits under the lock
    let v = Verify::open(&set, os).unwrap();
    std::thread::scope(|s| {
        for t in 0..4u64 {
            let (v, file) = (&v, &file);
            s.spawn(move || assert!(v.range(file, t * 4096, i - t * 4096).is_ok()));
        }
    });
    // a hash list that is not the manifest's
    list[40] ^= 1;
    rig.put("os.img.hashes", &list);
    assert!(Verify::open(&set, os).is_err());
    list[40] ^= 1;
    rig.put("os.img.hashes", &list);

    // the trust cache sidecar: three of Apple's modules and the owner's (two CDHashes, the repeated one once)
    let blob = set.trustcaches().unwrap();
    assert!(xnuloader::boot::trustcaches_valid(&blob));
    assert!(blob[0] == 4 && blob.len() == 4 + 16 + (24 + 72) + (24 + 48) + (24 + 24) + (24 + 48));
    let mine = &blob[blob.len() - 72..];
    assert!(mine[0] == 2 && mine[20] == 2 && mine[24] == 0x01 && mine[48] == 0xff && mine[44] == 2);
    let payload = im4p_payload(&trust[0], b"trst").unwrap();
    assert!(payload.len() == 96 && payload[0] == 2);
    assert!(im4p_payload(&trust[0], b"trca").is_none());
    assert!(im4p_payload(&trust[0][..trust[0].len() - 1], b"trst").is_none()); // cut short
    assert!(im4p_payload(&trust[0][..3], b"trst").is_none());
    // a trust cache that is not the manifest's
    let last = trust[1].len() - 1;
    trust[1][last] ^= 1;
    rig.put("trust/system.trustcache", &trust[1]);
    assert_eq!(set.trustcaches(), Err("a file of the image set is not the one manifest.json lists"));
    trust[1][last] ^= 1;
    rig.put("trust/system.trustcache", &trust[1]);

    // The runtime's boot addon (addon.rs): trustcaches_trusting puts its CDHashes in the owner module, sorted with the
    // set's and once even when both name one. An image built without the plug-in has no owner-trust.json at all; its
    // CDHashes still make the module.
    let mut repeated = [0u8; 20];
    repeated[0] = 0xff;
    repeated[19] = 0xaa;
    let extra = [[0x42u8; 20], repeated, [0x42u8; 20]];
    let owner = set.owner_module(&extra).unwrap().unwrap();
    assert!(owner[0] == 2 && le::u32(&owner, 20) == 3);
    assert!(owner[24] == 0x01 && owner[48] == 0x42 && owner[72] == 0xff);
    let blob = set.trustcaches_trusting(&extra).unwrap();
    assert!(xnuloader::boot::trustcaches_valid(&blob));
    assert!(blob[0] == 4 && blob.len() == 4 + 16 + (24 + 72) + (24 + 48) + (24 + 24) + (24 + 72));
    let mut bare = set.clone();
    bare.files.retain(|f| f.path != "overlay/userland/owner-trust.json");
    let owner = bare.owner_module(&extra).unwrap().unwrap();
    assert!(le::u32(&owner, 20) == 2 && owner[24] == 0x42 && owner[48] == 0xff);
    assert_eq!(bare.trustcaches_trusting(&[]).unwrap()[0], 3);
    assert_eq!(bare.trustcaches_trusting(&extra).unwrap()[0], 4);

    // the device tree properties
    let mut node = Node::new("virtio-mmio-0");
    describe(
        &mut node,
        &DiskIdentity {
            role: "os".into(),
            verified: Some(ABC),
            volume_uuid: os.volume_uuid.clone(),
            ..Default::default()
        },
    );
    assert!(node.get("oneios,role").is_some() && node.get("oneios,verified").map(|p| p.value.len()) == Some(32));
    assert!(node.get("oneios,volume-uuid").is_some() && node.get("oneios,bytes").is_none());
    let mut bare = Node::new("virtio-mmio-1");
    describe(&mut bare, &DiskIdentity::default());
    assert!(bare.get("oneios,role").is_none());

    // manifests that must be refused, and a folder without one
    for refused in [
        "{\"format\": 2, \"name\": \"oneios-image\", \"build\": \"x\", \"files\": []}",
        "{\"format\": 1, \"name\": \"other\", \"build\": \"x\", \"files\": []}",
        "{\"format\": 1, \"name\": \"oneios-image\", \"build\": \"x\", \"files\": []}",
        "{\"format\": 1, \"name\": \"oneios-image\", \"build\": \"x\", \"files\": [{\"path\": \"../etc\", \"size\": 1, \"sha256\": \"00\"}]}",
        "{\"format\": 1, \"name\": \"oneios-image\", \"build\": \"x\", \"files\": [{\"path\": \"a\", \"size\": 1, \"sha256\": \"zz\"}]}",
        "{\"format\": 1, \"name\": \"oneios-image\", \"build\": \"x\", \"files\": [",
        "",
    ] {
        rig.put("manifest.json", refused.as_bytes());
        assert!(ImageSet::load(&dirs).is_err(), "{refused}");
    }
    fs::remove_file(dir.join("manifest.json")).unwrap();
    assert_eq!(ImageSet::load(&dirs), Ok(None));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn json_and_paths() {
    let j = Json(
        b"{\"a\": [1, 2, {\"b\": \"c\"}], \"n\": 18446744073709551615, \"o\": 18446744073709551616, \"s\": \"x\\\"y\"}",
    );
    let root = Some(0);
    assert_eq!(j.elements(j.get(root, "a")).count(), 3);
    assert_eq!(j.u64(j.get(root, "n")), Some(u64::MAX));
    assert_eq!(j.u64(j.get(root, "o")), None); // past u64: refused
    assert_eq!(j.str(j.get(root, "s"), 64), None); // an escape is refused
    assert!(j.get(root, "missing").is_none() && j.skip(0, 0).is_some());
    assert!(path_ok("a/b.img") && path_ok("overlay/userland/bin/oneios-seed"));
    for bad in ["", "/a", "a//b", "a/", ".a", "a/../b", "a b", "a/.hidden"] {
        assert!(!path_ok(bad), "{bad:?}");
    }
}

/// ONEIOS_IMAGE_SET=DIR: a real set's every hash list opens, the first and last chunk of each image verify, and
/// Apple's trust caches make a sidecar the loader takes. Skipped (with the reason) without one.
#[test]
fn real_image_set() {
    let Some(real) = std::env::var("ONEIOS_IMAGE_SET").ok().filter(|r| !r.is_empty()) else {
        println!("real set: skipped (ONEIOS_IMAGE_SET=DIR names one, e.g. out/image/24A446)");
        return;
    };
    let set = ImageSet::load(&real).expect("the manifest").expect("a manifest.json");
    for role in ["os", "systemos", "app"] {
        let f = set.role(role).expect("the role's image");
        assert!(!f.volume_uuid.is_empty());
        let file = File::open(Path::new(&real).join(&f.path)).unwrap();
        let v = Verify::open(&set, f).unwrap();
        assert!(v.range(&file, 0, 4096).is_ok() && v.range(&file, f.size - 4096, 4096).is_ok());
        println!("real set: {} volume {}, first and last chunk verified", f.path, f.volume_uuid);
    }
    match set.trustcaches() {
        Ok(blob) => {
            assert!(xnuloader::boot::trustcaches_valid(&blob));
            println!("real set: trust cache sidecar, {} modules, {} bytes", blob[0], blob.len());
        }
        Err(e) => panic!("real set: trust caches: {e}"),
    }
}

#[test]
fn the_versions_line() {
    let dir = std::env::temp_dir().join(format!("test-versions-{}", std::process::id()));
    let (rt, img) = (dir.join("runtime"), dir.join("image"));
    fs::create_dir_all(&rt).unwrap();
    fs::create_dir_all(&img).unwrap();
    let (rts, imgs) = (rt.to_str().unwrap(), img.to_str().unwrap());
    // Nothing there (a bench run without a stage, an old script): every field "unknown", never a failure.
    assert_eq!(
        versions_line(rts, imgs, None),
        "versions: runtime unknown (git unknown), kernel unknown, kernel.kc unknown, image unknown unknown unknown format unknown, app unknown"
    );
    assert!(versions_line("", "", Some("")).ends_with("app unknown"));
    // runtime.json and manifest.json as package.sh and `oneios-tool image manifest` write them (indent 2).
    fs::write(
        rt.join("runtime.json"),
        "{\n  \"format\": 1,\n  \"name\": \"oneios-runtime\",\n  \"version\": \"0.1.19\",\n  \"git\": {\n    \"commit\": \
         \"973e8f7a1b2c3d4e5f60718293a4b5c6d7e8f901\",\n    \"dirty\": false\n  },\n  \"placeholder\": false,\n  \"kernel\": \
         {\n    \"path\": \"kernel/xnu.macho\",\n    \"build_id\": \"xnu-12377.121.6-ios-27.0.1-24A446-c889b14b24f3\"\n  },\n  \
         \"files\": [\n    {\n      \"path\": \"bin/xnuhost\",\n      \"sha256\": \"00\"\n    }\n  ]\n}\n",
    )
    .unwrap();
    fs::write(
        img.join("manifest.json"),
        "{\n  \"format\": 1,\n  \"name\": \"oneios-image\",\n  \"os\": \"iOS\",\n  \"version\": \"27.0.1\",\n  \"build\": \
         \"24A446\",\n  \"kernel\": \"kernel.kc\",\n  \"files\": [\n    {\n      \"path\": \"data.seed\",\n      \"sha256\": \
         \"1111111111111111111111111111111111111111111111111111111111111111\"\n    },\n    {\n      \"path\": \"kernel.kc\",\n      \
         \"size\": 93323264,\n      \"sha256\": \"a28fedfdf52437f486ed8fb223398b17e333bdf6c8f9ff43e4b355a219a1c0d0\",\n      \
         \"role\": \"kernel\"\n    }\n  ]\n}\n",
    )
    .unwrap();
    let line = versions_line(rts, imgs, Some("208"));
    println!("{line}");
    assert_eq!(
        line,
        "versions: runtime 0.1.19 (git 973e8f7a1b2c), kernel xnu-12377.121.6-ios-27.0.1-24A446-c889b14b24f3, \
         kernel.kc a28fedfdf524, image iOS 27.0.1 24A446 format 1, app versionCode 208"
    );
    // A versionCode that is not a number is not printed; a dirty tree, a runtime and an image without a kernel.
    assert!(versions_line(rts, imgs, Some("20 8; rm")).ends_with("app unknown"));
    fs::write(
        rt.join("runtime.json"),
        r#"{"version": "0.1.19-dev", "git": {"commit": "973e8f7a1b2c3d4e5f60718293a4b5c6d7e8f901", "dirty": true}, "kernel": null}"#,
    )
    .unwrap();
    fs::write(
        img.join("manifest.json"),
        r#"{"format": 1, "os": "iOS", "version": "27.0.1", "build": "24A446", "kernel": null, "files": []}"#,
    )
    .unwrap();
    assert_eq!(
        versions_line(rts, imgs, Some("207")),
        "versions: runtime 0.1.19-dev (git 973e8f7a1b2c dirty), kernel none, kernel.kc none, image iOS 27.0.1 24A446 format 1, app versionCode 207"
    );
    // Bad files: not JSON, a value with a space, a kernel.kc the files do not list.
    fs::write(rt.join("runtime.json"), "{\"version\": \"0.1.19\", \"git\": ").unwrap();
    fs::write(
        img.join("manifest.json"),
        r#"{"format": "1", "os": "i OS", "build": "24A446", "kernel": "kernel.kc", "files": [{"path": "os.img"}]}"#,
    )
    .unwrap();
    assert_eq!(
        versions_line(rts, imgs, None),
        "versions: runtime unknown (git unknown), kernel unknown, kernel.kc unknown, image unknown unknown 24A446 format unknown, app unknown"
    );
    let _ = fs::remove_dir_all(&dir);
}
