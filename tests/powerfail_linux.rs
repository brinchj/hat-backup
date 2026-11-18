//! Power-loss check for `FileBackend::store` on Linux.
//!
//! `drop_caches` does not discard dirty pages, so it cannot tell a synced
//! blob from one that is only in the page cache. This test uses a
//! device-mapper linear disk instead:
//!
//! 1. `store()` on an ext4 filesystem on that disk.
//! 2. Write a sibling file and do not sync it.
//! 3. `dmsetup suspend --noflush`, then copy the backing image.
//! 4. Mount the copy. That is the disk after power loss: pages that never
//!    reached the device are gone, and the journal replays what did.
//!
//! Pass: the blob from `store()` is intact, and the unsynced sibling is not.
//! If the sibling survives, writeback ran and the result does not prove the
//! sync. The test fails closed in that case.
//!
//! Needs root, `losetup`, `dmsetup`, and `mkfs.ext4`. Ignored by `cargo test`.
//! `scripts/linux-powerfail.sh` runs it in a privileged container.

#![cfg(target_os = "linux")]

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use hat::backend::{FileBackend, StoreBackend};
use hat::crypto::CipherText;

#[test]
#[ignore = "needs root, loop, and device-mapper; scripts/linux-powerfail.sh"]
fn store_survives_simulated_power_loss() {
    let _writeback = Sysctl::set("dirty_writeback_centisecs", "0");
    let _expire = Sysctl::set("dirty_expire_centisecs", "360000");

    let mut disk = Disk::create();
    let blobs = disk.mount.join("blobs");
    fs::create_dir_all(&blobs).unwrap();

    let payload = vec![0x5Au8; 8192];
    let backend = FileBackend::new(blobs.clone());
    backend
        .store(b"blob", CipherText::new(payload.clone()), Box::new(|| {}))
        .expect("store");

    // After store() returns. A periodic flusher would make this file durable
    // too, which is why writeback is disabled above.
    let control = vec![0xA5u8; 256 * 1024];
    {
        let mut file = File::create(blobs.join("unsynced.bin")).unwrap();
        file.write_all(&control).unwrap();
    }

    let crash_blobs = disk.crash_and_mount();
    let crash = FileBackend::new(crash_blobs.clone());
    match crash.retrieve(b"blob") {
        Ok(Some(got)) => assert_eq!(
            got, payload,
            "blob on the crash image is truncated or changed"
        ),
        Ok(None) => panic!(
            "store() returned but the blob is not on the crash image; dir has {:?}",
            dir_names(&crash_blobs)
        ),
        Err(e) => panic!("reading blob from crash image: {e}"),
    }

    match fs::read(crash_blobs.join("unsynced.bin")) {
        Err(_) => {}
        Ok(got) if got == control => panic!(
            "power-loss harness is invalid: the unsynced file survived, so writeback \
             reached the device and store() survival does not prove the sync"
        ),
        Ok(got) => panic!(
            "power-loss harness is invalid: {} unsynced bytes reached the device",
            got.len()
        ),
    }
}

fn dir_names(path: &Path) -> Vec<String> {
    fs::read_dir(path)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

struct Sysctl {
    path: PathBuf,
    old: String,
}

impl Sysctl {
    fn set(name: &str, value: &str) -> Sysctl {
        let path = PathBuf::from(format!("/proc/sys/vm/{name}"));
        let old = fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!("read {path:?}: {e}. The harness needs to disable writeback or a pass is meaningless")
        });
        fs::write(&path, value).unwrap_or_else(|e| {
            panic!("write {path:?}: {e}. Run privileged so dirty writeback can be turned off")
        });
        Sysctl { path, old }
    }
}

impl Drop for Sysctl {
    fn drop(&mut self) {
        let _ = fs::write(&self.path, self.old.trim());
    }
}

struct Disk {
    dir: PathBuf,
    img: PathBuf,
    crash_img: PathBuf,
    loop_dev: Option<String>,
    dm_name: String,
    dm_created: bool,
    mount: PathBuf,
    mounted: bool,
    crash_mount: PathBuf,
    crash_mounted: bool,
    suspended: bool,
}

impl Disk {
    fn create() -> Disk {
        let uid = command("id", &["-u"]);
        assert_eq!(uid.trim(), "0", "power-loss test must run as root");

        let dir = PathBuf::from(format!("/tmp/hat-powerfail-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let img = dir.join("disk.img");
        let crash_img = dir.join("crash.img");
        let mount = dir.join("mnt");
        let crash_mount = dir.join("crash-mnt");
        fs::create_dir_all(&mount).unwrap();
        fs::create_dir_all(&crash_mount).unwrap();

        let file = File::create(&img).unwrap();
        // 96 MiB is enough for an ext4 journal plus the blob.
        file.set_len(96 * 1024 * 1024).unwrap();
        drop(file);

        let dm_name = format!("hatpf{}", std::process::id());
        let mut disk = Disk {
            dir,
            img,
            crash_img,
            loop_dev: None,
            dm_name,
            dm_created: false,
            mount,
            mounted: false,
            crash_mount,
            crash_mounted: false,
            suspended: false,
        };

        // Own the loop device before anything else can panic and leak it.
        let loop_dev = command("losetup", &["--find", "--show", disk.img.to_str().unwrap()]);
        let loop_dev = loop_dev.trim().to_string();
        assert!(
            loop_dev.starts_with("/dev/"),
            "losetup returned {loop_dev:?}"
        );
        disk.loop_dev = Some(loop_dev);

        let sectors = command("blockdev", &["--getsz", disk.loop_dev.as_deref().unwrap()]);
        let table = format!(
            "0 {} linear {} 0",
            sectors.trim(),
            disk.loop_dev.as_deref().unwrap()
        );
        command("dmsetup", &["create", &disk.dm_name, "--table", &table]);
        disk.dm_created = true;
        // Containers have no udev, so dmsetup create does not add the node.
        let _ = fs::create_dir_all("/dev/mapper");
        command("dmsetup", &["mknodes", &disk.dm_name]);

        let dev = format!("/dev/mapper/{}", disk.dm_name);
        command("mkfs.ext4", &["-F", "-q", &dev]);
        command(
            "mount",
            &["-o", "data=ordered", &dev, disk.mount.to_str().unwrap()],
        );
        disk.mounted = true;
        disk
    }

    /// Suspend without flushing, snapshot the backing file, mount that snapshot.
    fn crash_and_mount(&mut self) -> PathBuf {
        command("dmsetup", &["suspend", "--noflush", &self.dm_name]);
        self.suspended = true;
        fs::copy(&self.img, &self.crash_img).expect("copy backing image");
        command("dmsetup", &["resume", &self.dm_name]);
        self.suspended = false;

        command("umount", &[self.mount.to_str().unwrap()]);
        self.mounted = false;

        // Journal replay on this mount is the recovery a reboot would do.
        command(
            "mount",
            &[
                "-o",
                "loop,data=ordered",
                self.crash_img.to_str().unwrap(),
                self.crash_mount.to_str().unwrap(),
            ],
        );
        self.crash_mounted = true;
        self.crash_mount.join("blobs")
    }
}

impl Drop for Disk {
    fn drop(&mut self) {
        if self.suspended {
            let _ = Command::new("dmsetup")
                .args(["resume", &self.dm_name])
                .status();
            self.suspended = false;
        }
        if self.crash_mounted {
            let _ = Command::new("umount").arg(&self.crash_mount).status();
        }
        if self.mounted {
            let _ = Command::new("umount").arg(&self.mount).status();
        }
        if self.dm_created {
            let _ = Command::new("dmsetup")
                .args(["remove", "--force", &self.dm_name])
                .status();
        }
        if let Some(loop_dev) = self.loop_dev.take() {
            let _ = Command::new("losetup").args(["-d", &loop_dev]).status();
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn command(bin: &str, args: &[&str]) -> String {
    let out = Command::new(bin)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {bin}: {e}"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "{bin} {args:?} exited {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.status
    );
    stdout.into_owned()
}
