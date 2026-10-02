use dustagent::application::package::{install, load, pack};
use flate2::{Compression, write::GzEncoder};
use std::fs;

fn app(root: &std::path::Path) {
    fs::create_dir_all(root.join("skills/crawl/references")).unwrap();
    fs::write(root.join("app.json"), r#"{"package":{"name":"crawler","version":"1.0.0","dust_version":">=0.1.0"},"skills":[],"system_prompt":"test"}"#).unwrap();
    fs::write(
        root.join("skills/crawl/SKILL.md"),
        "---\nname: crawl\ndescription: Crawl\n---\nObserve",
    )
    .unwrap();
    fs::write(root.join("skills/crawl/references/check.md"), "Coverage").unwrap();
}
#[test]
fn directory_archive_and_install_have_same_content_identity() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    app(&source);
    let archive = pack(&source, Some(&temp.path().join("crawler.dustpkg"))).unwrap();
    let direct = load(source.to_str().unwrap(), temp.path()).unwrap();
    let unpacked = load(archive.to_str().unwrap(), temp.path()).unwrap();
    assert_eq!(direct.digest, unpacked.digest);
    assert_ne!(direct.root, unpacked.root);
    let store = temp.path().join("store");
    let installed = install(&archive, &store).unwrap();
    assert_eq!(
        direct.digest,
        load(installed.to_str().unwrap(), temp.path())
            .unwrap()
            .digest
    );
    assert!(install(&archive, &store).is_err());
    assert!(pack(&source, Some(&archive)).is_err());
}
#[test]
fn metadata_and_compatibility_are_checked() {
    let temp = tempfile::tempdir().unwrap();
    app(temp.path());
    for metadata in [
        r#"{"name":"../escape","version":"1.0.0"}"#,
        r#"{"name":"test","version":"latest"}"#,
        r#"{"name":"test","version":"1.0.0","dust_version":">=99.0.0"}"#,
    ] {
        fs::write(
            temp.path().join("app.json"),
            format!("{{\"package\":{metadata}}}"),
        )
        .unwrap();
        assert!(load(temp.path().to_str().unwrap(), temp.path()).is_err());
    }
    fs::write(temp.path().join("app.json"), "{}").unwrap();
    assert!(load(temp.path().to_str().unwrap(), temp.path()).is_err());
}
#[test]
fn archive_links_duplicates_and_missing_manifest_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    for (label, kind, duplicate) in [
        ("link", tar::EntryType::Symlink, false),
        ("hardlink", tar::EntryType::Link, false),
        ("duplicate", tar::EntryType::Regular, true),
        ("missing", tar::EntryType::Regular, false),
    ] {
        let path = temp.path().join(format!("{label}.dustpkg"));
        let encoder = GzEncoder::new(fs::File::create(&path).unwrap(), Compression::default());
        let mut builder = tar::Builder::new(encoder);
        for _ in 0..if duplicate { 2 } else { 1 } {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(kind);
            header.set_mode(0o600);
            header.set_size(0);
            if kind.is_symlink() || kind.is_hard_link() {
                header.set_link_name("/tmp/outside").unwrap();
            }
            header.set_cksum();
            builder.append_data(&mut header, "file", &[][..]).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
        assert!(
            load(path.to_str().unwrap(), temp.path()).is_err(),
            "{label}"
        );
    }
}
#[cfg(unix)]
#[test]
fn directory_symlinks_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    app(temp.path());
    std::os::unix::fs::symlink("/etc/passwd", temp.path().join("external")).unwrap();
    assert!(load(temp.path().to_str().unwrap(), temp.path()).is_err());
}
#[test]
fn legacy_and_local_named_package_resolution_remain_supported() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("apps")).unwrap();
    fs::write(temp.path().join("apps/legacy.json"), "{}").unwrap();
    assert!(load("legacy", temp.path()).unwrap().digest.is_none());
    app(&temp.path().join("apps/crawler"));
    assert!(load("crawler", temp.path()).unwrap().digest.is_some());
}

#[test]
fn raw_archive_traversal_and_oversized_entries_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    for (label, name, size, kind) in [
        ("parent", "../outside", 0, tar::EntryType::Regular),
        ("absolute", "/outside", 0, tar::EntryType::Regular),
        (
            "oversized",
            "large",
            17 * 1024 * 1024,
            tar::EntryType::Regular,
        ),
        ("metadata", "extended", 0, tar::EntryType::GNULongName),
    ] {
        let path = temp.path().join(format!("{label}.dustpkg"));
        let mut header = tar::Header::new_ustar();
        header.set_mode(0o600);
        header.set_size(size);
        header.set_entry_type(kind);
        header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
        header.set_cksum();
        let mut raw = header.as_bytes().to_vec();
        raw.extend_from_slice(&[0u8; 1024]);
        use std::io::Write;
        let mut compressed =
            GzEncoder::new(fs::File::create(&path).unwrap(), Compression::default());
        compressed.write_all(&raw).unwrap();
        compressed.finish().unwrap();
        assert!(
            load(path.to_str().unwrap(), temp.path()).is_err(),
            "{label}"
        );
    }
}
