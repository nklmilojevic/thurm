use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;

use thurm_client::ClientError;
use thurm_client::stream::{self, StreamReader};

use super::*;

/// A pane in `dir` whose input goes to `dir/out.txt` (`cat`), line by line.
fn cat_pane(c: &Client, dir: &Path) -> PaneId {
    let r = c
        .request(Request::CreatePane(CreatePane {
            command: Some(vec!["/bin/sh".into(), "-c".into(), "cat > out.txt".into()]),
            cwd: Some(dir.display().to_string()),
            ..Default::default()
        }))
        .unwrap();
    let Response::PaneCreated { pane } = r else {
        panic!("{r:?}")
    };
    pane
}

/// Ends the pane's input and waits for `cat` to have written it all.
fn typed(c: &Client, pane: PaneId, dir: &Path) -> String {
    c.request(Request::Input {
        pane,
        data: b"\n\x04".to_vec(),
    })
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let alive = matches!(
            c.request(Request::PaneInfo { pane }),
            Ok(Response::PaneInfo(ref info)) if info.alive
        );
        if !alive {
            return std::fs::read_to_string(dir.join("out.txt")).unwrap_or_default();
        }
        assert!(Instant::now() < deadline, "cat did not finish");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn work_dir(env: &Env) -> PathBuf {
    let dir = env.dir.join("work");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Sends file `index` of `count` in batch `id`: `data`, though `size` is announced.
fn send(
    env: &Env,
    c: &Client,
    pane: PaneId,
    (id, index, count): (&str, u32, u32),
    name: &str,
    size: u64,
    data: &[u8],
) -> StreamReader {
    let batch = UploadBatch {
        id: id.into(),
        index,
        count,
    };
    let ticket = c.upload_begin(pane, name, size, Some(batch)).unwrap();
    let (r, mut w) = stream::open(&env.socket, &ticket.ticket).unwrap();
    w.write_all(data).unwrap();
    w.finish().unwrap();
    r
}

/// The batch directories under the daemon's drops for `pane`.
fn batches(env: &Env, pane: PaneId) -> Vec<PathBuf> {
    let dir = env.dir.join("state/drops").join(pane.to_string());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries.flatten().map(|e| e.path()).collect()
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn the_daemon_says_it_takes_files() {
    let env = Env::new("transfer-cap");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    assert!(c.has_capability(CAP_TRANSFER));
}

#[test]
fn a_batch_lands_and_its_paths_are_typed_in_order() {
    let env = Env::new("transfer-batch");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    let big: Vec<u8> = (0..300_000).map(|i| (i % 251) as u8).collect();
    let files: [(&str, &[u8]); 3] = [
        ("first.txt", b"one"),
        ("a b.png", &big),
        ("/elsewhere/third", b""),
    ];
    let mut readers: Vec<StreamReader> = files
        .iter()
        .enumerate()
        .map(|(i, (name, data))| {
            send(
                &env,
                &c,
                pane,
                ("b1", i as u32, 3),
                name,
                data.len() as u64,
                data,
            )
        })
        .collect();
    // Every stream closes Ok once the whole batch is in.
    for r in readers.drain(..) {
        r.wait_closed().unwrap();
    }
    let Ok([batch]) = <[PathBuf; 1]>::try_from(batches(&env, pane)) else {
        panic!("{:?}", batches(&env, pane))
    };
    let expected = [
        batch.join("first.txt"),
        batch.join("a b.png"),
        batch.join("third"),
    ];
    for (path, (_, data)) in expected.iter().zip(files) {
        assert_eq!(std::fs::read(path).unwrap(), data, "{}", path.display());
        assert_eq!(mode(path), 0o600);
    }
    assert_eq!(mode(&batch), 0o700);
    assert_eq!(mode(&env.dir.join("state/drops")), 0o700);
    let line = format!(
        "{} '{}' {} \n",
        expected[0].display(),
        expected[1].display(),
        expected[2].display()
    );
    assert_eq!(typed(&c, pane, &dir), line);
}

#[test]
fn a_failed_file_types_nothing_and_leaves_nothing() {
    let env = Env::new("transfer-fail");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    let first = send(&env, &c, pane, ("b", 0, 2), "ok.txt", 2, b"ok");
    // Fewer bytes than announced.
    let second = send(&env, &c, pane, ("b", 1, 2), "short.txt", 10, b"short");
    for r in [first, second] {
        let e = r.wait_closed().unwrap_err().to_string();
        assert!(e.contains("5 of the 10 bytes"), "{e}");
    }
    assert!(batches(&env, pane).is_empty(), "{:?}", batches(&env, pane));
    // A file of the failed batch sent later is refused with the reason.
    let later = c.upload_begin(
        pane,
        "late.txt",
        1,
        Some(UploadBatch {
            id: "b".into(),
            index: 1,
            count: 2,
        }),
    );
    assert!(
        matches!(&later, Err(ClientError::Daemon(e)) if e.contains("bytes announced")),
        "{later:?}"
    );
    // More bytes than announced fail too.
    let over = send(&env, &c, pane, ("c", 0, 1), "over.txt", 1, b"over");
    let e = over.wait_closed().unwrap_err().to_string();
    assert!(e.contains("more than the 1 bytes"), "{e}");
    // An aborted stream fails its batch.
    let ticket = c.upload_begin(pane, "cut.txt", 100, None).unwrap();
    let (r, mut w) = stream::open(&env.socket, &ticket.ticket).unwrap();
    w.write_all(b"partial").unwrap();
    drop(w);
    assert!(r.wait_closed().is_err());
    assert!(batches(&env, pane).is_empty(), "{:?}", batches(&env, pane));
    c.request(Request::Input {
        pane,
        data: b"marker".to_vec(),
    })
    .unwrap();
    assert_eq!(typed(&c, pane, &dir), "marker\n");
}

#[test]
fn a_batch_is_dropped_when_its_client_goes() {
    let env = Env::new("transfer-gone");
    let _daemon = env.start();
    let (keeper, _krx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&keeper, &dir);
    let (c, _rx) = env.connect();
    let first = send(&env, &c, pane, ("g", 0, 2), "one.txt", 3, b"one");
    let deadline = Instant::now() + Duration::from_secs(10);
    while batches(&env, pane)
        .first()
        .is_none_or(|b| !b.join("one.txt").exists())
    {
        assert!(Instant::now() < deadline, "the file did not land");
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(c);
    let e = first.wait_closed().unwrap_err().to_string();
    assert!(e.contains("went away"), "{e}");
    assert!(batches(&env, pane).is_empty());
    assert_eq!(typed(&keeper, pane, &dir), "\n");
}

#[test]
fn names_are_cleaned_and_kept_apart() {
    let env = Env::new("transfer-names");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    let names = ["../../evil\n.txt", "evil.txt", "\u{1b}[2J"];
    let readers: Vec<StreamReader> = names
        .iter()
        .enumerate()
        .map(|(i, n)| send(&env, &c, pane, ("n", i as u32, 3), n, 1, b"x"))
        .collect();
    for r in readers {
        r.wait_closed().unwrap();
    }
    let Ok([batch]) = <[PathBuf; 1]>::try_from(batches(&env, pane)) else {
        panic!()
    };
    let mut got: Vec<String> = std::fs::read_dir(&batch)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != ".parts" && n != ".done")
        .collect();
    got.sort();
    assert_eq!(got, ["[2J", "evil-2.txt", "evil.txt"]);
}

#[test]
fn an_upload_that_would_fill_the_disk_is_refused() {
    let env = Env::new("transfer-full");
    let _daemon = env.start_with(&[
        ("THURM_TRANSFER_FREE_BYTES", Path::new("1073741900")),
        ("THURM_TRANSFER_BLOCK", Path::new("1")),
    ]);
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    // 1 GiB must stay free, and the directories count (a byte each here): 73 bytes fit.
    c.upload_begin(pane, "fits", 73, None).unwrap();
    let e = c.upload_begin(pane, "too-much", 1000, None).unwrap_err();
    assert!(e.to_string().contains("free space"), "{e}");
    let e = c.upload_begin(pane, "huge", 5 << 30, None).unwrap_err();
    assert!(e.to_string().contains("too large"), "{e}");
}

#[test]
fn a_file_downloads_and_is_not_sent_again_unchanged() {
    let env = Env::new("transfer-download");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let data: Vec<u8> = (0..200_000).map(|i| (i % 241) as u8).collect();
    let file = dir.join("data.bin");
    write_settled(&file, &data);
    let pane = cat_pane(&c, &dir);

    let info = c.download_begin(pane, "data.bin", None).unwrap();
    assert_eq!(info.name, "data.bin");
    assert!(info.path.ends_with("/work/data.bin"), "{}", info.path);
    assert_eq!(info.size, data.len() as u64);
    let version = info
        .version
        .clone()
        .expect("a file unchanged for a minute has a version");
    let (mut r, mut w) = stream::open(&env.socket, info.ticket.as_deref().unwrap()).unwrap();
    let mut got = Vec::new();
    r.read_to_end(&mut got).unwrap();
    assert!(got == data, "{} of {} bytes", got.len(), data.len());
    w.finish().unwrap();
    r.wait_closed().unwrap();

    let again = c.download_begin(pane, "data.bin", Some(&version)).unwrap();
    assert!(again.not_modified);
    assert_eq!(again.ticket, None);
    let other = c.download_begin(pane, "data.bin", Some("0")).unwrap();
    assert!(!other.not_modified && other.ticket.is_some());

    let e = c.download_begin(pane, ".", None).unwrap_err();
    assert!(e.to_string().contains("not a regular file"), "{e}");
    let e = c.download_begin(pane, "missing", None).unwrap_err();
    // Nothing there says so in a way a client can tell from other failures.
    assert!(e.to_string().ends_with(DOWNLOAD_NOT_FOUND), "{e}");
    let e = c.download_begin(pane, "data.bin/inside", None).unwrap_err();
    assert!(e.to_string().ends_with(DOWNLOAD_NOT_FOUND), "{e}");
}

/// Writes `data` to `path`, modified a minute ago, and waits out the second after its change
/// time in which it has no version.
fn write_settled(path: &Path, data: &[u8]) {
    std::fs::write(path, data).unwrap();
    set_modified(path, std::time::SystemTime::now() - Duration::from_secs(60));
    std::thread::sleep(Duration::from_millis(1100));
}

fn set_modified(path: &Path, when: std::time::SystemTime) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(when)
        .unwrap();
}

#[test]
fn a_file_rewritten_with_its_old_modification_time_is_sent_again() {
    let env = Env::new("transfer-ctime");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let file = dir.join("same.txt");
    write_settled(&file, b"before");
    let pane = cat_pane(&c, &dir);
    let first = c.download_begin(pane, "same.txt", None).unwrap();
    let version = first.version.clone().unwrap();
    let modified = std::fs::metadata(&file).unwrap().modified().unwrap();
    // Same length, same modification time: only the change time tells.
    std::fs::write(&file, b"after!").unwrap();
    set_modified(&file, modified);
    std::thread::sleep(Duration::from_millis(1100));
    let again = c.download_begin(pane, "same.txt", Some(&version)).unwrap();
    assert!(!again.not_modified, "{again:?}");
    assert!(again.ticket.is_some());
    assert_ne!(again.version.as_deref(), Some(version.as_str()));
}

#[test]
fn a_file_changed_after_it_was_asked_for_is_not_sent() {
    let env = Env::new("transfer-changed");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let file = dir.join("moving.txt");
    write_settled(&file, b"version one");
    let pane = cat_pane(&c, &dir);
    let info = c.download_begin(pane, "moving.txt", None).unwrap();
    // Edited in place, to the same length, before the stream opens.
    std::fs::write(&file, b"version two").unwrap();
    let (mut r, mut w) = stream::open(&env.socket, info.ticket.as_deref().unwrap()).unwrap();
    let mut got = Vec::new();
    let e = r.read_to_end(&mut got).unwrap_err();
    assert!(
        e.to_string().contains("changed since it was asked for"),
        "{e}"
    );
    assert!(got.is_empty());
    let _ = w.finish();
}

#[test]
fn kept_uploads_stay_within_the_limit_batch_and_all() {
    let env = Env::new("transfer-kept");
    let _daemon = env.start_with(&[
        ("THURM_TRANSFER_MAX_KEPT", Path::new("14")),
        ("THURM_TRANSFER_BLOCK", Path::new("1")),
    ]);
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    send(&env, &c, pane, ("a", 0, 1), "a.txt", 4, b"aaaa")
        .wait_closed()
        .unwrap();
    let [first] = <[PathBuf; 1]>::try_from(batches(&env, pane)).unwrap();
    // A byte a directory and the completion record here, each batch with two directories:
    // "a" keeps 7, and "b" takes 11 more; the older drop makes room.
    send(&env, &c, pane, ("b", 0, 1), "b.txt", 8, b"bbbbbbbb")
        .wait_closed()
        .unwrap();
    assert!(!first.exists());
    // One batch whose files together are over the limit is refused at the file that does not
    // fit, before any of its bytes.
    let first_file = send(&env, &c, pane, ("c", 0, 2), "c1.txt", 6, b"cccccc");
    let e = c
        .upload_begin(
            pane,
            "c2.txt",
            6,
            Some(UploadBatch {
                id: "c".into(),
                index: 1,
                count: 2,
            }),
        )
        .unwrap_err();
    assert!(e.to_string().contains("does not fit"), "{e}");
    // "c" took the room the kept "b" held.
    let has = |name: &str| batches(&env, pane).iter().any(|b| b.join(name).exists());
    let deadline = Instant::now() + Duration::from_secs(10);
    while !has("c1.txt") {
        assert!(Instant::now() < deadline, "c1.txt did not land");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!has("b.txt"));
    drop(first_file);
    // Beside the 9 of "c", still open, a batch that fits whole (5) goes.
    let readers: Vec<StreamReader> = (0..2)
        .map(|i| send(&env, &c, pane, ("d", i, 2), "d.txt", 1, b"d"))
        .collect();
    for r in readers {
        r.wait_closed().unwrap();
    }
    assert!(has("d.txt") && has("d-2.txt") && has("c1.txt"));
}

#[test]
fn cancelling_after_the_last_bytes_drops_the_batch() {
    let env = Env::new("transfer-abort");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    let ticket = c
        .upload_begin(
            pane,
            "one.txt",
            3,
            Some(UploadBatch {
                id: "x".into(),
                index: 0,
                count: 2,
            }),
        )
        .unwrap();
    let (r, mut w) = stream::open(&env.socket, &ticket.ticket).unwrap();
    w.write_all(b"one").unwrap();
    w.finish().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while batches(&env, pane)
        .first()
        .is_none_or(|b| !b.join("one.txt").exists())
    {
        assert!(Instant::now() < deadline, "the file did not land");
        std::thread::sleep(Duration::from_millis(20));
    }
    // Everything sent and finished, the batch waiting for its second file: Cancel.
    w.close(thurm_proto::stream::StreamClose::Aborted).unwrap();
    let e = r.wait_closed().unwrap_err().to_string();
    assert!(e.contains("cancelled"), "{e}");
    assert!(batches(&env, pane).is_empty());
    assert_eq!(typed(&c, pane, &dir), "\n");
}

#[test]
fn a_refused_upload_deletes_nothing_kept() {
    let env = Env::new("transfer-refused-keeps");
    let _daemon = env.start_with(&[
        ("THURM_TRANSFER_MAX_KEPT", Path::new("11")),
        ("THURM_TRANSFER_BLOCK", Path::new("1")),
    ]);
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    send(&env, &c, pane, ("a", 0, 1), "a.txt", 4, b"aaaa")
        .wait_closed()
        .unwrap();
    let [kept] = <[PathBuf; 1]>::try_from(batches(&env, pane)).unwrap();
    // 11 bytes and their batch's 3 can never fit in 11: refused, and the kept drop stays.
    let e = c.upload_begin(pane, "big.txt", 11, None).unwrap_err();
    assert!(e.to_string().contains("does not fit"), "{e}");
    assert!(kept.join("a.txt").exists());
    // The same for a batch whose files together can never fit (a byte a directory and the
    // completion record here: the kept drop takes 7, and "b" with its first file 4).
    let first = send(&env, &c, pane, ("b", 0, 2), "b1.txt", 1, b"b");
    let e = c
        .upload_begin(
            pane,
            "b2.txt",
            10,
            Some(UploadBatch {
                id: "b".into(),
                index: 1,
                count: 2,
            }),
        )
        .unwrap_err();
    assert!(e.to_string().contains("does not fit"), "{e}");
    assert!(kept.join("a.txt").exists());
    drop(first);
}

#[test]
fn a_later_file_refused_ends_the_batch_at_once() {
    use thurm_remote::transfer::{self, Cancel, Host, TransferError, Upload};
    let env = Env::new("transfer-later-refused");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    let batch = |index| {
        Some(UploadBatch {
            id: "r".into(),
            index,
            count: 2,
        })
    };
    // The first file sent and landed, its stream kept as a sender keeps it.
    let ticket = c.upload_begin(pane, "small.txt", 5, batch(0)).unwrap();
    let (r0, mut w0) = stream::open(&env.socket, &ticket.ticket).unwrap();
    w0.write_all(b"small").unwrap();
    w0.finish().unwrap();
    eventually(5, "the first file did not land", || {
        batches(&env, pane)
            .first()
            .is_some_and(|b| b.join("small.txt").exists())
    });
    let [landed] = <[PathBuf; 1]>::try_from(batches(&env, pane)).unwrap();
    // The second is refused; the sender aborts the first stream, and the batch goes now, not
    // when it would have stalled.
    let e = c
        .upload_begin(pane, "huge.bin", 5 << 30, batch(1))
        .unwrap_err();
    assert!(e.to_string().contains("too large"), "{e}");
    w0.close(thurm_proto::stream::StreamClose::Aborted).unwrap();
    assert!(r0.wait_closed().is_err());
    eventually(5, "the landed batch was kept", || !landed.exists());
    assert_eq!(typed(&c, pane, &dir), "\n");

    // `upload` does the same on its own: a refused later file leaves nothing.
    let small = dir.join("small.txt");
    std::fs::write(&small, b"small").unwrap();
    let huge = dir.join("huge.bin");
    std::fs::File::create(&huge)
        .unwrap()
        .set_len(5 << 30)
        .unwrap();
    let (c2, _rx2) = env.connect();
    let pane2 = cat_pane(&c2, &dir);
    let host = Host {
        client: &c2,
        socket: &env.socket,
    };
    let files = [Upload::Path(small), Upload::Path(huge)];
    // The first file's bytes seen on the host before the second is asked for, so the batch
    // that goes is one that was there.
    let mut seen = None;
    let e = transfer::upload(host, pane2, &files, &Cancel::default(), |p| {
        if p.done == 5 && seen.is_none() {
            eventually(5, "the first file did not arrive", || {
                batches(&env, pane2).first().is_some_and(|b| {
                    std::fs::metadata(b.join(".parts/0")).is_ok_and(|m| m.len() == 5)
                })
            });
            seen = batches(&env, pane2).into_iter().next();
        }
    })
    .unwrap_err();
    assert!(
        matches!(&e, TransferError::Remote(why) if why.contains("too large")),
        "{e}"
    );
    let seen = seen.expect("the first file was never sent whole");
    eventually(5, "the batch was kept", || !seen.exists());
    assert!(batches(&env, pane2).is_empty());
    assert_eq!(typed(&c2, pane2, &dir), "\n");
}

#[test]
fn a_batch_whose_sender_went_before_its_paste_types_nothing() {
    let env = Env::new("transfer-gone-before-paste");
    // Time between the last file landing and the paste, for the sender to go in.
    let _daemon = env.start_with(&[("THURM_TRANSFER_PASTE_DELAY_MS", Path::new("500"))]);
    let (keeper, _krx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&keeper, &dir);
    let (c, _rx) = env.connect();
    let r = send(&env, &c, pane, ("p", 0, 1), "one.txt", 3, b"one");
    eventually(5, "the file did not land", || {
        batches(&env, pane)
            .first()
            .is_some_and(|b| b.join("one.txt").exists())
    });
    drop(c);
    let e = r.wait_closed().unwrap_err().to_string();
    assert!(e.contains("went away"), "{e}");
    eventually(5, "the batch was kept", || batches(&env, pane).is_empty());
    assert_eq!(typed(&keeper, pane, &dir), "\n");
}

#[test]
fn a_reservation_refused_for_its_batch_deletes_nothing_kept() {
    let env = Env::new("transfer-dup-keeps");
    let _daemon = env.start_with(&[
        ("THURM_TRANSFER_MAX_KEPT", Path::new("11")),
        ("THURM_TRANSFER_BLOCK", Path::new("1")),
    ]);
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    // 4 bytes, two directories and the completion record of a byte each here: 7 kept.
    send(&env, &c, pane, ("a", 0, 1), "a.txt", 4, b"aaaa")
        .wait_closed()
        .unwrap();
    let [kept] = <[PathBuf; 1]>::try_from(batches(&env, pane)).unwrap();
    let batch = |index, count| {
        Some(UploadBatch {
            id: "b".into(),
            index,
            count,
        })
    };
    // 1 byte, its batch's two directories and record: 4, beside the 7 kept.
    c.upload_begin(pane, "b1.txt", 1, batch(0, 2)).unwrap();
    // The same file again, and the batch with another count: each would have needed room the
    // kept drop holds, and is refused without taking it.
    let e = c.upload_begin(pane, "b1.txt", 3, batch(0, 2)).unwrap_err();
    assert!(e.to_string().contains("already on its way"), "{e}");
    let e = c.upload_begin(pane, "b2.txt", 3, batch(1, 3)).unwrap_err();
    assert!(e.to_string().contains("not 3"), "{e}");
    assert!(kept.join("a.txt").exists());
}

#[test]
fn a_kept_drop_that_cannot_be_deleted_still_counts() {
    // Permissions do not stop root.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let env = Env::new("transfer-undeletable");
    let _daemon = env.start_with(&[
        ("THURM_TRANSFER_MAX_KEPT", Path::new("10")),
        ("THURM_TRANSFER_BLOCK", Path::new("1")),
    ]);
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    send(&env, &c, pane, ("a", 0, 1), "a.txt", 6, b"aaaaaa")
        .wait_closed()
        .unwrap();
    let [kept] = <[PathBuf; 1]>::try_from(batches(&env, pane)).unwrap();
    std::fs::set_permissions(&kept, std::fs::Permissions::from_mode(0o500)).unwrap();
    // 6 kept that stay, and 6 more: over 10, so refused rather than counted as freed.
    let e = c.upload_begin(pane, "b.txt", 6, None).unwrap_err();
    std::fs::set_permissions(&kept, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(e.to_string().contains("does not fit"), "{e}");
    assert!(kept.join("a.txt").exists());
    // Deletable again, it makes room.
    c.upload_begin(pane, "b.txt", 6, None).unwrap();
    assert!(!kept.exists());
}

/// Waits up to `secs` for `done`.
fn eventually(secs: u64, what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn kept_uploads_expire_without_another_upload() {
    let env = Env::new("transfer-expire");
    // Drops an earlier daemon left: one long past its time, one just made.
    let old = env.dir.join("state/drops/9/old");
    let fresh = env.dir.join("state/drops/9/fresh");
    for dir in [&old, &fresh] {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("f.txt"), b"x").unwrap();
    }
    set_modified_all(
        &old,
        std::time::SystemTime::now() - Duration::from_secs(3600),
    );
    let _daemon = env.start_with(&[
        ("THURM_TRANSFER_KEEP_MS", Path::new("2000")),
        ("THURM_TRANSFER_PRUNE_MS", Path::new("100")),
    ]);
    eventually(5, "the old drop was kept after the start", || !old.exists());
    assert!(fresh.exists());
    eventually(5, "the fresh drop outlived its time", || !fresh.exists());
    // One sent now goes after its time too, with nothing asked of the daemon meanwhile.
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    send(&env, &c, pane, ("a", 0, 1), "a.txt", 1, b"a")
        .wait_closed()
        .unwrap();
    let [sent] = <[PathBuf; 1]>::try_from(batches(&env, pane)).unwrap();
    eventually(6, "the sent drop outlived its time", || !sent.exists());
}

#[test]
fn kept_drops_make_room_for_the_free_space_floor() {
    let env = Env::new("transfer-floor");
    // 1 GiB must stay free; this disk has 10 bytes more than that with nothing kept.
    let _daemon = env.start_with(&[
        ("THURM_TRANSFER_FREE_BYTES", Path::new("1073741834")),
        ("THURM_TRANSFER_BLOCK", Path::new("1")),
    ]);
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    send(&env, &c, pane, ("a", 0, 1), "a.txt", 6, b"aaaaaa")
        .wait_closed()
        .unwrap();
    let [kept] = <[PathBuf; 1]>::try_from(batches(&env, pane)).unwrap();
    // 20 bytes would not fit even with the kept drop gone: refused, and it stays.
    let e = c.upload_begin(pane, "big.txt", 20, None).unwrap_err();
    assert!(e.to_string().contains("not enough free space"), "{e}");
    assert!(kept.join("a.txt").exists());
    // 6 fit once it goes: it goes.
    send(&env, &c, pane, ("b", 0, 1), "b.txt", 6, b"bbbbbb")
        .wait_closed()
        .unwrap();
    assert!(!kept.exists());
}

#[test]
fn a_failed_batch_stops_its_writers_and_leaves_nothing() {
    let env = Env::new("transfer-writers");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    let batch = |index| {
        Some(UploadBatch {
            id: "w".into(),
            index,
            count: 2,
        })
    };
    // The first file half sent, its stream left open.
    let t0 = c.upload_begin(pane, "big.bin", 1 << 20, batch(0)).unwrap();
    let (r0, mut w0) = stream::open(&env.socket, &t0.ticket).unwrap();
    w0.write_all(&vec![7u8; 4096]).unwrap();
    eventually(5, "the first bytes did not arrive", || {
        batches(&env, pane)
            .first()
            .is_some_and(|b| std::fs::metadata(b.join(".parts/0")).is_ok_and(|m| m.len() == 4096))
    });
    // The second is aborted: the batch fails, and the first stream ends without waiting for
    // more of its bytes.
    let t1 = c.upload_begin(pane, "small.bin", 1, batch(1)).unwrap();
    let (r1, w1) = stream::open(&env.socket, &t1.ticket).unwrap();
    w1.close(thurm_proto::stream::StreamClose::Aborted).unwrap();
    assert!(r1.wait_closed().is_err());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(r0.wait_closed().map_err(|e| e.to_string()));
    });
    let closed = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the failed batch's writer kept taking bytes");
    assert!(closed.is_err(), "{closed:?}");
    // More bytes go nowhere.
    let _ = w0.write_all(&vec![7u8; 1 << 16]);
    eventually(5, "the failed batch was kept", || {
        batches(&env, pane).is_empty()
    });
}

#[test]
fn empty_files_count_against_what_is_kept() {
    let env = Env::new("transfer-empty");
    // Room for two kept drops of an empty file: a block for it and for each of its two
    // directories, 4 KiB each.
    let _daemon = env.start_with(&[("THURM_TRANSFER_MAX_KEPT", Path::new("24576"))]);
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    for i in 0..6 {
        send(&env, &c, pane, (&format!("e{i}"), 0, 1), "empty", 0, b"")
            .wait_closed()
            .unwrap();
        assert!(batches(&env, pane).len() <= 2, "{:?}", batches(&env, pane));
    }
}

#[test]
fn an_upload_leaves_the_disk_files_to_spare() {
    let env = Env::new("transfer-inodes");
    // 10 000 files must stay; this disk takes 3 more with nothing kept, too few for one upload.
    let _daemon = env.start_with(&[("THURM_TRANSFER_FREE_FILES", Path::new("10003"))]);
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    let e = c.upload_begin(pane, "a.txt", 1, None).unwrap_err();
    assert!(e.to_string().contains("too few more files"), "{e}");
}

#[test]
fn a_panes_first_upload_counts_every_file_it_makes() {
    // A pane's first upload makes its pane's directory, the batch's two, the file and its
    // part, and the batch's record of being complete: six. Five more is one too few.
    let env = Env::new("transfer-inodes-first");
    let _daemon = env.start_with(&[("THURM_TRANSFER_FREE_FILES", Path::new("10005"))]);
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    let e = c.upload_begin(pane, "a.txt", 1, None).unwrap_err();
    assert!(e.to_string().contains("too few more files"), "{e}");
    assert!(batches(&env, pane).is_empty());
}

#[test]
fn a_panes_first_upload_takes_the_last_files_above_the_floor() {
    let env = Env::new("transfer-inodes-exact");
    let _daemon = env.start_with(&[("THURM_TRANSFER_FREE_FILES", Path::new("10006"))]);
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    send(&env, &c, pane, ("a", 0, 1), "a.txt", 1, b"a")
        .wait_closed()
        .unwrap();
    assert!(batches(&env, pane).iter().any(|b| b.join("a.txt").exists()));
}

#[test]
fn batches_left_incomplete_are_capped() {
    let env = Env::new("transfer-waiting");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    // The first file of each two-file batch, and never the second.
    let mut held = Vec::new();
    for i in 0..8 {
        held.push(send(
            &env,
            &c,
            pane,
            (&format!("w{i}"), 0, 2),
            "one",
            1,
            b"1",
        ));
    }
    let e = c
        .upload_begin(
            pane,
            "one",
            1,
            Some(UploadBatch {
                id: "w8".into(),
                index: 0,
                count: 2,
            }),
        )
        .unwrap_err();
    assert!(e.to_string().contains("not complete yet"), "{e}");
    // The open ones go on, and another app is not held up.
    c.upload_begin(
        pane,
        "two",
        1,
        Some(UploadBatch {
            id: "w0".into(),
            index: 1,
            count: 2,
        }),
    )
    .unwrap();
    let (other, _orx) = env.connect();
    other.upload_begin(pane, "x", 1, None).unwrap();
    drop(held);
}

#[test]
fn cancelling_the_last_file_before_its_paste_types_nothing() {
    let env = Env::new("transfer-cancel-before-paste");
    // Time between the last file landing and the paste, for the cancel to come in.
    let _daemon = env.start_with(&[("THURM_TRANSFER_PASTE_DELAY_MS", Path::new("500"))]);
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    let ticket = c.upload_begin(pane, "last.txt", 4, None).unwrap();
    let (r, mut w) = stream::open(&env.socket, &ticket.ticket).unwrap();
    w.write_all(b"last").unwrap();
    w.finish().unwrap();
    eventually(5, "the file did not land", || {
        batches(&env, pane)
            .first()
            .is_some_and(|b| b.join("last.txt").exists())
    });
    w.close(thurm_proto::stream::StreamClose::Aborted).unwrap();
    let e = r.wait_closed().unwrap_err().to_string();
    assert!(e.contains("cancelled"), "{e}");
    eventually(5, "the cancelled batch was kept", || {
        batches(&env, pane).is_empty()
    });
    assert_eq!(typed(&c, pane, &dir), "\n");
}

/// Sets the modification time of `path` (a file or a directory), and of everything in it.
fn set_modified_all(path: &Path, when: std::time::SystemTime) {
    if path.is_dir() {
        for e in std::fs::read_dir(path).unwrap().flatten() {
            set_modified_all(&e.path(), when);
        }
        std::fs::File::open(path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    } else {
        set_modified(path, when);
    }
}

#[test]
fn a_slow_batch_is_kept_from_when_it_was_complete() {
    let env = Env::new("transfer-slow");
    let _daemon = env.start_with(&[
        ("THURM_TRANSFER_KEEP_MS", Path::new("2000")),
        ("THURM_TRANSFER_PRUNE_MS", Path::new("100")),
    ]);
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    // Its first file, then its second after longer than drops are kept.
    let first = send(&env, &c, pane, ("s", 0, 2), "one.txt", 3, b"one");
    std::thread::sleep(Duration::from_millis(2500));
    let last = send(&env, &c, pane, ("s", 1, 2), "two.txt", 3, b"two");
    last.wait_closed().unwrap();
    drop(first);
    let [batch] = <[PathBuf; 1]>::try_from(batches(&env, pane)).unwrap();
    // Whatever its directory's dates say, it is aged from when it was complete.
    set_modified_all(
        &batch,
        std::time::SystemTime::now() - Duration::from_secs(3600),
    );
    std::thread::sleep(Duration::from_millis(800));
    assert!(batch.join("one.txt").exists() && batch.join("two.txt").exists());
    eventually(6, "the batch outlived its time", || !batch.exists());
}

#[test]
fn drops_left_by_an_earlier_daemon_keep_their_age() {
    let env = Env::new("transfer-restart");
    let hour_ago = std::time::SystemTime::now() - Duration::from_secs(3600);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let drop_dir = |name: &str| {
        let dir = env.dir.join("state/drops/9").join(name);
        std::fs::create_dir_all(dir.join(".parts")).unwrap();
        std::fs::write(dir.join("f.txt"), b"x").unwrap();
        dir
    };
    // Complete a moment ago, its dates long past.
    let done = drop_dir("done");
    std::fs::write(done.join(".done"), now_ms.to_string()).unwrap();
    set_modified_all(&done, hour_ago);
    // Never complete: aged from its last write, a moment ago for one, long ago for the other.
    let partial_fresh = drop_dir("partial-fresh");
    set_modified_all(&partial_fresh, hour_ago);
    std::fs::write(partial_fresh.join(".parts/0"), b"part").unwrap();
    let partial_old = drop_dir("partial-old");
    set_modified_all(&partial_old, hour_ago);
    let _daemon = env.start_with(&[
        ("THURM_TRANSFER_KEEP_MS", Path::new("2000")),
        ("THURM_TRANSFER_PRUNE_MS", Path::new("100")),
    ]);
    eventually(5, "the old partial drop was kept", || !partial_old.exists());
    assert!(done.exists() && partial_fresh.exists());
    eventually(6, "the complete drop outlived its time", || !done.exists());
    eventually(6, "the partial drop outlived its time", || {
        !partial_fresh.exists()
    });
}

#[test]
fn a_batch_that_cannot_be_made_deletes_nothing_kept() {
    let env = Env::new("transfer-no-dir");
    let _daemon = env.start_with(&[
        ("THURM_TRANSFER_MAX_KEPT", Path::new("10")),
        ("THURM_TRANSFER_BLOCK", Path::new("1")),
    ]);
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    let other = cat_pane(&c, &dir);
    send(&env, &c, pane, ("a", 0, 1), "a.txt", 6, b"aaaaaa")
        .wait_closed()
        .unwrap();
    let [kept] = <[PathBuf; 1]>::try_from(batches(&env, pane)).unwrap();
    // The other pane's directory cannot be made (a file is in its place), and admitting its
    // upload would have needed the kept drop's room.
    std::fs::write(env.dir.join("state/drops").join(other.to_string()), b"").unwrap();
    let e = c.upload_begin(other, "b.txt", 6, None).unwrap_err();
    assert!(e.to_string().contains("cannot create"), "{e}");
    assert!(kept.join("a.txt").exists());
}

#[test]
fn drops_past_their_time_make_room_at_once() {
    let env = Env::new("transfer-expired-room");
    // 10 bytes over the floor with nothing kept, a byte a directory and record here.
    let _daemon = env.start_with(&[
        ("THURM_TRANSFER_FREE_BYTES", Path::new("1073741834")),
        ("THURM_TRANSFER_BLOCK", Path::new("1")),
    ]);
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    send(&env, &c, pane, ("a", 0, 1), "a.txt", 6, b"aaaaaa")
        .wait_closed()
        .unwrap();
    let [kept] = <[PathBuf; 1]>::try_from(batches(&env, pane)).unwrap();
    // It is past its time, and holds the room the next one needs.
    std::fs::write(kept.join(".done"), "1000").unwrap();
    send(&env, &c, pane, ("b", 0, 1), "b.txt", 6, b"bbbbbb")
        .wait_closed()
        .unwrap();
    assert!(!kept.exists());
}

#[test]
fn a_download_keeps_its_slot_until_its_stream_ends() {
    let env = Env::new("transfer-download-slots");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    std::fs::write(dir.join("small.txt"), b"small").unwrap();
    let pane = cat_pane(&c, &dir);
    // Four sent whole, their callers never saying they have them.
    let mut held = Vec::new();
    for _ in 0..4 {
        let info = c.download_begin(pane, "small.txt", None).unwrap();
        let (mut r, w) = stream::open(&env.socket, info.ticket.as_deref().unwrap()).unwrap();
        let mut got = Vec::new();
        r.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"small");
        held.push((r, w));
    }
    let e = c.download_begin(pane, "small.txt", None).unwrap_err();
    assert!(e.to_string().contains("too many downloads"), "{e}");
    // One answered, its slot is free again.
    let (r, mut w) = held.pop().unwrap();
    w.finish().unwrap();
    r.wait_closed().unwrap();
    eventually(5, "the slot was not freed", || {
        c.download_begin(pane, "small.txt", None).is_ok()
    });
    drop(held);
}

#[test]
fn a_download_to_where_nothing_can_be_written_fails_cleanly() {
    use thurm_remote::transfer::{self, Cancel, Host, TransferError};
    let env = Env::new("transfer-no-dest");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    // Larger than what a connection holds: a stream left sending it would block.
    std::fs::write(dir.join("big.bin"), vec![7u8; 8 << 20]).unwrap();
    let pane = cat_pane(&c, &dir);
    let host = Host {
        client: &c,
        socket: &env.socket,
    };
    let dest = dir.join("missing/big.bin");
    let (tx, rx) = std::sync::mpsc::channel();
    let socket = env.socket.clone();
    let c2 = c.clone();
    std::thread::spawn(move || {
        let host = Host {
            client: &c2,
            socket: &socket,
        };
        let got = transfer::download(
            host,
            pane,
            "big.bin",
            &dest,
            None,
            None,
            &Cancel::default(),
            |_| {},
        );
        let _ = tx.send(got);
    });
    let got = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the failed download did not come back");
    assert!(matches!(got, Err(TransferError::Local(_))), "{got:?}");
    // Nothing is left where it would have gone, and the next download goes.
    assert!(!dir.join("missing").exists());
    let ok = dir.join("big-copy.bin");
    transfer::download(
        host,
        pane,
        "big.bin",
        &ok,
        None,
        None,
        &Cancel::default(),
        |_| {},
    )
    .unwrap();
    assert_eq!(std::fs::metadata(&ok).unwrap().len(), 8 << 20);
}

#[test]
fn kept_drops_give_back_files_for_the_floor() {
    let env = Env::new("transfer-inodes-room");
    // 10 000 files must stay; this disk takes 8 more with nothing kept: the pane's first
    // upload's six, and then not another's five until the first gives its own back.
    let _daemon = env.start_with(&[("THURM_TRANSFER_FREE_FILES", Path::new("10008"))]);
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    send(&env, &c, pane, ("a", 0, 1), "a.txt", 1, b"a")
        .wait_closed()
        .unwrap();
    let [kept] = <[PathBuf; 1]>::try_from(batches(&env, pane)).unwrap();
    send(&env, &c, pane, ("b", 0, 1), "b.txt", 1, b"b")
        .wait_closed()
        .unwrap();
    assert!(!kept.exists());
    assert!(batches(&env, pane).iter().any(|b| b.join("b.txt").exists()));
}

#[test]
fn files_on_their_way_count_before_they_land() {
    let env = Env::new("transfer-waiting-files");
    let _daemon = env.start_with(&[("THURM_TRANSFER_MAX_WAITING_FILES", Path::new("4"))]);
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let panes: Vec<PaneId> = (0..2).map(|_| cat_pane(&c, &dir)).collect();
    let (other, _orx) = env.connect();
    // Reserved, none sent yet, in two panes and from two apps.
    let reserve = |c: &Client, pane, id: &str, index| {
        c.upload_begin(
            pane,
            "f",
            1,
            Some(UploadBatch {
                id: id.into(),
                index,
                count: 8,
            }),
        )
    };
    reserve(&c, panes[0], "x", 0).unwrap();
    reserve(&c, panes[0], "x", 1).unwrap();
    reserve(&other, panes[1], "y", 0).unwrap();
    reserve(&other, panes[1], "y", 1).unwrap();
    let e = reserve(&c, panes[1], "z", 0).unwrap_err();
    assert!(e.to_string().contains("not complete yet"), "{e}");
}

#[test]
fn a_pane_whose_program_ended_takes_no_files() {
    let env = Env::new("transfer-dead-pane");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let r = c
        .request(Request::CreatePane(CreatePane {
            command: Some(vec!["/bin/sh".into(), "-c".into(), "exit 0".into()]),
            cwd: Some(dir.display().to_string()),
            hold: true,
            ..Default::default()
        }))
        .unwrap();
    let Response::PaneCreated { pane } = r else {
        panic!("{r:?}")
    };
    eventually(10, "the program did not end", || {
        !matches!(
            c.request(Request::PaneInfo { pane }),
            Ok(Response::PaneInfo(ref info)) if info.alive
        )
    });
    let e = c.upload_begin(pane, "a.txt", 1, None).unwrap_err();
    assert!(e.to_string().contains("closed"), "{e}");
}

#[test]
fn a_download_nobody_answers_gives_its_slot_back() {
    let env = Env::new("transfer-ack");
    let _daemon = env.start_with(&[("THURM_TRANSFER_ACK_MS", Path::new("300"))]);
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    std::fs::write(dir.join("small.txt"), b"small").unwrap();
    let pane = cat_pane(&c, &dir);
    // Four read whole and never answered, their connections held open.
    let mut held = Vec::new();
    for _ in 0..4 {
        let info = c.download_begin(pane, "small.txt", None).unwrap();
        let (mut r, w) = stream::open(&env.socket, info.ticket.as_deref().unwrap()).unwrap();
        let mut got = Vec::new();
        r.read_to_end(&mut got).unwrap();
        held.push((r, w));
    }
    assert!(c.download_begin(pane, "small.txt", None).is_err());
    eventually(5, "the slots were held for ever", || {
        c.download_begin(pane, "small.txt", None).is_ok()
    });
    drop(held);
}

#[test]
fn a_download_into_a_folder_takes_a_free_name_and_touches_nothing_there() {
    use thurm_remote::transfer::{self, Cancel, Host, TransferError};
    let env = Env::new("transfer-to-folder");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    std::fs::write(dir.join("report.pdf"), b"the report").unwrap();
    std::fs::write(dir.join("big.bin"), vec![1u8; 4 << 20]).unwrap();
    let pane = cat_pane(&c, &dir);
    let host = Host {
        client: &c,
        socket: &env.socket,
    };
    let downloads = env.dir.join("downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    let get = |path: &str, name: &str| {
        transfer::download_to_folder(
            host,
            pane,
            path,
            &downloads,
            name,
            &Cancel::default(),
            |_| {},
        )
    };
    // Under its name, and the next time beside it, the first left as it was.
    let (info, first) = get("report.pdf", "report.pdf").unwrap();
    assert_eq!(info.name, "report.pdf");
    assert_eq!(first, downloads.join("report.pdf"));
    assert_eq!(std::fs::read(&first).unwrap(), b"the report");
    std::fs::write(&first, b"edited since").unwrap();
    let (_, second) = get("report.pdf", "report.pdf").unwrap();
    assert_eq!(second, downloads.join("report 2.pdf"));
    assert_eq!(std::fs::read(&second).unwrap(), b"the report");
    assert_eq!(std::fs::read(&first).unwrap(), b"edited since");
    // A link under the name is neither followed nor replaced.
    let linked = downloads.join("linked.pdf");
    std::os::unix::fs::symlink(dir.join("elsewhere"), &linked).unwrap();
    let (_, saved) = get("report.pdf", "linked.pdf").unwrap();
    assert_eq!(saved, downloads.join("linked 2.pdf"));
    assert!(std::fs::symlink_metadata(&linked).unwrap().is_symlink());
    assert!(!dir.join("elsewhere").exists());
    // A name that would leave the folder is not taken.
    let (_, saved) = get("report.pdf", "../escape.pdf").unwrap();
    assert_eq!(saved, downloads.join("file"));
    // Its private folder's name pointed elsewhere before the file goes in: the file it was
    // written to goes in all the same, and nothing elsewhere is taken or touched.
    let elsewhere = env.dir.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("part"), b"theirs").unwrap();
    let moved = env.dir.join("moved");
    let mut pointed = false;
    let (_, saved) = transfer::download_to_folder(
        host,
        pane,
        "report.pdf",
        &downloads,
        "pointed.pdf",
        &Cancel::default(),
        |p| {
            if p.total > 0 && p.done == p.total && !pointed {
                pointed = true;
                let private = std::fs::read_dir(&downloads)
                    .unwrap()
                    .flatten()
                    .map(|e| e.path())
                    .find(|p| {
                        p.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .starts_with(".thurm-")
                    })
                    .unwrap();
                std::fs::rename(&private, &moved).unwrap();
                std::os::unix::fs::symlink(&elsewhere, &private).unwrap();
            }
        },
    )
    .unwrap();
    assert!(pointed);
    assert_eq!(saved, downloads.join("pointed.pdf"));
    assert_eq!(std::fs::read(&saved).unwrap(), b"the report");
    assert_eq!(std::fs::read(elsewhere.join("part")).unwrap(), b"theirs");
    // The link left in its place is not the folder made, so it stays; so does the folder.
    for link in std::fs::read_dir(&downloads).unwrap().flatten() {
        if link.file_name().to_string_lossy().starts_with(".thurm-") {
            assert!(link.file_type().unwrap().is_symlink());
            std::fs::remove_file(link.path()).unwrap();
        }
    }
    assert!(moved.is_dir());
    // A download that fails, or is cancelled, leaves nothing.
    let before: std::collections::BTreeSet<_> = std::fs::read_dir(&downloads)
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    let e = get("nothing-here", "missing.pdf").unwrap_err();
    assert!(matches!(e, TransferError::NotFound(_)), "{e}");
    let cancel = Cancel::default();
    let stop = cancel.clone();
    let e =
        transfer::download_to_folder(host, pane, "big.bin", &downloads, "big.bin", &cancel, |p| {
            if p.done > 0 {
                stop.cancel();
            }
        })
        .unwrap_err();
    assert!(matches!(e, TransferError::Cancelled), "{e}");
    let after: std::collections::BTreeSet<_> = std::fs::read_dir(&downloads)
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(before, after);
}

#[test]
fn a_pane_another_terminal_holds_takes_no_files_from_the_start() {
    let env = Env::new("transfer-attached");
    let _daemon = env.start();
    let (owner, _orx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&owner, &dir);
    owner
        .request(Request::AttachTerminal {
            pane,
            size: PaneSize::default(),
        })
        .unwrap();
    // Refused before a byte, not once the files have come.
    let (c, _rx) = env.connect();
    let e = c.upload_begin(pane, "a.txt", 1, None).unwrap_err();
    assert!(e.to_string().contains("attached terminal"), "{e}");
    owner.request(Request::DetachTerminal { pane }).unwrap();
    c.upload_begin(pane, "a.txt", 1, None).unwrap();
}

#[test]
fn a_sender_that_stops_mid_file_is_not_waited_on() {
    let env = Env::new("transfer-stalled-writer");
    // Batches are idle after twice the ticket's life (a second at least): 2 s here.
    let _daemon = env.start_with(&[("THURM_STREAM_TICKET_MS", Path::new("500"))]);
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    let ticket = c.upload_begin(pane, "slow.bin", 100, None).unwrap();
    let (r, mut w) = stream::open(&env.socket, &ticket.ticket).unwrap();
    w.write_all(b"only some").unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(r.wait_closed().map_err(|e| e.to_string()));
    });
    let closed = rx
        .recv_timeout(Duration::from_secs(8))
        .expect("the stalled stream was waited on");
    assert!(closed.is_err(), "{closed:?}");
    eventually(5, "the stalled batch was kept", || {
        batches(&env, pane).is_empty()
    });
    drop(w);
}

#[test]
fn names_a_case_blind_disk_would_take_for_one_are_kept_apart() {
    let env = Env::new("transfer-case");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    let readers: Vec<StreamReader> = [
        ("Photo.jpg", b"one"),
        ("photo.jpg", b"two"),
        (".Done", b"rec"),
    ]
    .iter()
    .enumerate()
    .map(|(i, (n, d))| send(&env, &c, pane, ("k", i as u32, 3), n, 3, *d))
    .collect();
    for r in readers {
        r.wait_closed().unwrap();
    }
    let [batch] = <[PathBuf; 1]>::try_from(batches(&env, pane)).unwrap();
    assert_eq!(std::fs::read(batch.join("Photo.jpg")).unwrap(), b"one");
    assert_eq!(std::fs::read(batch.join("photo-2.jpg")).unwrap(), b"two");
    assert_eq!(std::fs::read(batch.join(".Done-2")).unwrap(), b"rec");
    // The batch's own record is its own.
    assert!(
        std::fs::read_to_string(batch.join(".done"))
            .unwrap()
            .parse::<u64>()
            .is_ok()
    );
}

#[test]
fn a_refused_upload_gives_its_ticket_back() {
    let env = Env::new("transfer-ticket-back");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    // Its batch's directory cannot be made (a file is in its pane's place).
    std::fs::create_dir_all(env.dir.join("state/drops")).unwrap();
    std::fs::write(env.dir.join("state/drops").join(pane.to_string()), b"").unwrap();
    // Retried past the 64 tickets a client may hold, it still says why.
    for _ in 0..70 {
        let e = c.upload_begin(pane, "a.txt", 1, None).unwrap_err();
        assert!(e.to_string().contains("cannot create"), "{e}");
    }
}

#[test]
fn drops_on_their_way_count_toward_the_drops_kept() {
    let env = Env::new("transfer-max-drops");
    let _daemon = env.start_with(&[("THURM_TRANSFER_MAX_DROPS", Path::new("2"))]);
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    send(&env, &c, pane, ("k", 0, 1), "kept.txt", 1, b"k")
        .wait_closed()
        .unwrap();
    let [kept] = <[PathBuf; 1]>::try_from(batches(&env, pane)).unwrap();
    // One on its way, and then another: three drops on disk would be one too many.
    let _waiting = send(&env, &c, pane, ("w", 0, 2), "w.txt", 1, b"w");
    c.upload_begin(pane, "n.txt", 1, None).unwrap();
    assert!(!kept.exists());
}

#[test]
fn a_file_whose_ticket_ran_out_unused_ends_its_batch() {
    let env = Env::new("transfer-lapsed");
    // Tickets last 300 ms; batches stall after 2 s.
    let _daemon = env.start_with(&[("THURM_STREAM_TICKET_MS", Path::new("300"))]);
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    // Reserved, its stream never opened.
    c.upload_begin(
        pane,
        "never.txt",
        1,
        Some(UploadBatch {
            id: "n".into(),
            index: 0,
            count: 2,
        }),
    )
    .unwrap();
    let [batch] = <[PathBuf; 1]>::try_from(batches(&env, pane)).unwrap();
    std::thread::sleep(Duration::from_millis(1200));
    // The next upload looks: the batch is gone before it would have stalled.
    c.upload_begin(pane, "next.txt", 1, None).unwrap();
    assert!(!batch.exists());
}

#[test]
fn names_a_disk_takes_for_one_by_their_unicode_form_are_kept_apart() {
    let env = Env::new("transfer-nfd");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    // The same name composed and decomposed: one file on a disk that ignores the form.
    let readers: Vec<StreamReader> = [("caf\u{e9}.txt", b"nfc"), ("cafe\u{301}.txt", b"nfd")]
        .iter()
        .enumerate()
        .map(|(i, (n, d))| send(&env, &c, pane, ("u", i as u32, 2), n, 3, *d))
        .collect();
    for r in readers {
        r.wait_closed().unwrap();
    }
    let [batch] = <[PathBuf; 1]>::try_from(batches(&env, pane)).unwrap();
    let mut kept: Vec<Vec<u8>> = std::fs::read_dir(&batch)
        .unwrap()
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| std::fs::read(e.path()).unwrap())
        .collect();
    kept.sort();
    assert_eq!(kept, [b"nfc".to_vec(), b"nfd".to_vec()]);
}

#[test]
fn downloads_refused_for_their_size_use_up_no_tickets() {
    use thurm_remote::transfer::{self, Cancel, Host, TransferError};
    let env = Env::new("transfer-too-large");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    std::fs::write(dir.join("big.bin"), vec![1u8; 100]).unwrap();
    let pane = cat_pane(&c, &dir);
    let host = Host {
        client: &c,
        socket: &env.socket,
    };
    // More than the 64 tickets a client may hold, each refused for its size.
    for i in 0..70 {
        let e = transfer::download(
            host,
            pane,
            "big.bin",
            &dir.join(format!("never-{i}")),
            None,
            Some(10),
            &Cancel::default(),
            |_| {},
        )
        .unwrap_err();
        assert!(matches!(e, TransferError::TooLarge { .. }), "{e}");
    }
    // The next one is not refused for tickets.
    c.upload_begin(pane, "a.txt", 1, None).unwrap();
}

#[test]
fn a_stream_closed_once_all_is_said_is_no_cancel() {
    let env = Env::new("transfer-close-ok");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    let ticket = c
        .upload_begin(
            pane,
            "one.txt",
            3,
            Some(UploadBatch {
                id: "o".into(),
                index: 0,
                count: 2,
            }),
        )
        .unwrap();
    let (_r, mut w) = stream::open(&env.socket, &ticket.ticket).unwrap();
    w.write_all(b"one").unwrap();
    w.finish().unwrap();
    // The first file's stream closed Ok, its file sent: the batch goes on.
    w.close(thurm_proto::stream::StreamClose::Ok).unwrap();
    send(&env, &c, pane, ("o", 1, 2), "two.txt", 3, b"two")
        .wait_closed()
        .unwrap();
    let [batch] = <[PathBuf; 1]>::try_from(batches(&env, pane)).unwrap();
    assert!(batch.join("one.txt").exists() && batch.join("two.txt").exists());
}

#[test]
fn drops_are_not_deleted_for_an_upload_they_cannot_make_room_for() {
    // Permissions do not stop root.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let env = Env::new("transfer-plan-room");
    let _daemon = env.start_with(&[
        ("THURM_TRANSFER_MAX_KEPT", Path::new("10")),
        ("THURM_TRANSFER_BLOCK", Path::new("1")),
    ]);
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    let other = cat_pane(&c, &dir);
    // Two kept drops of 5 each (2 bytes, two directories, the record): one cannot be deleted.
    send(&env, &c, pane, ("a", 0, 1), "a.txt", 2, b"aa")
        .wait_closed()
        .unwrap();
    send(&env, &c, other, ("b", 0, 1), "b.txt", 2, b"bb")
        .wait_closed()
        .unwrap();
    let [stuck] = <[PathBuf; 1]>::try_from(batches(&env, pane)).unwrap();
    let [free] = <[PathBuf; 1]>::try_from(batches(&env, other)).unwrap();
    std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o500)).unwrap();
    // 5 more and its two directories need 7 freed; the deletable drop frees 5: refused, and
    // it is not deleted for nothing.
    let e = c.upload_begin(other, "c.txt", 5, None).unwrap_err();
    std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(e.to_string().contains("does not fit"), "{e}");
    assert!(free.join("b.txt").exists());
    assert!(stuck.join("a.txt").exists());
}

#[test]
fn a_file_half_written_is_not_counted_twice_against_the_floor() {
    let env = Env::new("transfer-half-written");
    // The floor plus 1204 bytes with nothing kept (a byte a directory here).
    let _daemon = env.start_with(&[
        ("THURM_TRANSFER_FREE_BYTES", Path::new("1073743028")),
        ("THURM_TRANSFER_BLOCK", Path::new("1")),
    ]);
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    let batch = |index| {
        Some(UploadBatch {
            id: "h".into(),
            index,
            count: 2,
        })
    };
    // 900 of the first file's 1000 bytes written, its stream still open.
    let t = c.upload_begin(pane, "first.bin", 1000, batch(0)).unwrap();
    let (_r, mut w) = stream::open(&env.socket, &t.ticket).unwrap();
    w.write_all(&vec![1u8; 900]).unwrap();
    eventually(5, "the first bytes did not arrive", || {
        batches(&env, pane)
            .first()
            .is_some_and(|b| std::fs::metadata(b.join(".parts/0")).is_ok_and(|m| m.len() == 900))
    });
    // 300 free above the floor: the first file's last 100 and the second's 100 fit.
    c.upload_begin(pane, "second.bin", 100, batch(1)).unwrap();
}

#[test]
fn a_download_answered_with_anything_but_its_end_still_gives_its_slot_back() {
    let env = Env::new("transfer-ack-trickle");
    let _daemon = env.start_with(&[("THURM_TRANSFER_ACK_MS", Path::new("400"))]);
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    std::fs::write(dir.join("small.txt"), b"small").unwrap();
    let pane = cat_pane(&c, &dir);
    // Four read whole, their callers sending a byte now and then but never their end.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut tricklers = Vec::new();
    for _ in 0..4 {
        let info = c.download_begin(pane, "small.txt", None).unwrap();
        let (mut r, mut w) = stream::open(&env.socket, info.ticket.as_deref().unwrap()).unwrap();
        let mut got = Vec::new();
        r.read_to_end(&mut got).unwrap();
        let stop = stop.clone();
        tricklers.push(std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                if w.write_all(b".").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            drop(r);
        }));
    }
    assert!(c.download_begin(pane, "small.txt", None).is_err());
    // Within the deadline, whatever they send.
    eventually(2, "the slots were held by sending", || {
        c.download_begin(pane, "small.txt", None).is_ok()
    });
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    for t in tricklers {
        t.join().unwrap();
    }
}

#[test]
fn a_download_answered_by_a_frame_that_never_ends_still_gives_its_slot_back() {
    let env = Env::new("transfer-ack-partial");
    let _daemon = env.start_with(&[("THURM_TRANSFER_ACK_MS", Path::new("400"))]);
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    std::fs::write(dir.join("small.txt"), b"small").unwrap();
    let pane = cat_pane(&c, &dir);
    // Four read whole, their callers starting a frame of 1000 bytes and sending it a byte at a
    // time, each well within the deadline, the whole far beyond it.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut tricklers = Vec::new();
    for _ in 0..4 {
        let info = c.download_begin(pane, "small.txt", None).unwrap();
        let socket = std::os::unix::net::UnixStream::connect(&env.socket).unwrap();
        let mut w = socket.try_clone().unwrap();
        let (mut r, _framed) = stream::open_on(socket, info.ticket.as_deref().unwrap()).unwrap();
        let mut got = Vec::new();
        r.read_to_end(&mut got).unwrap();
        w.write_all(&1000u32.to_le_bytes()).unwrap();
        let stop = stop.clone();
        tricklers.push(std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                if w.write_all(&[0]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            drop((r, _framed));
        }));
    }
    assert!(c.download_begin(pane, "small.txt", None).is_err());
    eventually(2, "the slots were held by a frame never finished", || {
        c.download_begin(pane, "small.txt", None).is_ok()
    });
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    for t in tricklers {
        t.join().unwrap();
    }
}

#[test]
fn more_files_than_a_batch_holds_are_refused_before_any_goes() {
    use thurm_remote::transfer::{self, Cancel, Host, TransferError, Upload};
    let env = Env::new("transfer-257");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    let host = Host {
        client: &c,
        socket: &env.socket,
    };
    let files: Vec<Upload> = (0..257)
        .map(|i| Upload::Data {
            name: format!("f{i}"),
            data: b"x".to_vec(),
        })
        .collect();
    let e = transfer::upload(host, pane, &files, &Cancel::default(), |_| {}).unwrap_err();
    assert!(
        matches!(&e, TransferError::Local(why) if why.contains("at most 256")),
        "{e}"
    );
    assert!(batches(&env, pane).is_empty());
}

/// `thurm cp`: a file goes to a path its sender names and comes back, whole or not at all,
/// replacing a file only when told to.
#[test]
fn a_file_is_copied_to_a_named_path_and_back() {
    use thurm_remote::transfer::{self, Cancel, Host, TransferError};
    let env = Env::new("transfer-copy");
    let home = env.dir.join("home");
    std::fs::create_dir_all(home.join("inbox")).unwrap();
    let _daemon = env.start_with(&[("HOME", &home)]);
    let (c, _rx) = env.connect();
    assert!(c.has_capability(CAP_COPY));
    let host = Host {
        client: &c,
        socket: &env.socket,
    };
    let dir = work_dir(&env);
    let data: Vec<u8> = (0..300_000).map(|i| (i % 239) as u8).collect();
    let src = dir.join("data.bin");
    std::fs::write(&src, &data).unwrap();
    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o640)).unwrap();
    let cancel = Cancel::default();
    let to =
        |dest: &str, replace: bool| transfer::copy_to(host, &src, dest, replace, &cancel, |_| {});

    // Into a folder under its own name; `~` and relative paths are under the home directory.
    let mut seen = Vec::new();
    let (at, sent) =
        transfer::copy_to(host, &src, "inbox", false, &cancel, |p| seen.push(p)).unwrap();
    assert_eq!(sent, data.len() as u64);
    // Where it landed is told: the file in the folder, not the folder that was named.
    assert_eq!(Path::new(&at), home.join("inbox/data.bin"));
    assert_eq!(seen.last().map(|p| (p.done, p.total)), Some((sent, sent)));
    let landed = home.join("inbox/data.bin");
    assert!(std::fs::read(&landed).unwrap() == data);
    assert_eq!(mode(&landed), 0o640, "the file keeps its permission bits");
    let (at, _) = to("~/inbox/named.bin", false).unwrap();
    assert_eq!(Path::new(&at), home.join("inbox/named.bin"));
    assert!(std::fs::read(home.join("inbox/named.bin")).unwrap() == data);
    let abs = dir.join("abs.bin");
    to(&abs.display().to_string(), false).unwrap();
    assert!(std::fs::read(&abs).unwrap() == data);

    // A file that is there stays unless it is to be replaced; nothing else gives way.
    std::fs::write(&landed, b"old").unwrap();
    let e = to("inbox/", false).unwrap_err();
    assert!(e.to_string().contains("is there already"), "{e}");
    assert_eq!(std::fs::read(&landed).unwrap(), b"old");
    to("inbox/", true).unwrap();
    assert!(std::fs::read(&landed).unwrap() == data);
    std::fs::create_dir(home.join("inbox/data.bin.d")).unwrap();
    std::os::unix::fs::symlink(&abs, home.join("inbox/link")).unwrap();
    let e = to("inbox/link", true).unwrap_err();
    assert!(e.to_string().contains("not a regular file"), "{e}");
    let e = to("inbox/missing/", false).unwrap_err();
    assert!(e.to_string().contains("not a folder"), "{e}");
    let e = to("nowhere/data.bin", false).unwrap_err();
    assert!(e.to_string().contains("not a folder"), "{e}");
    let e = transfer::copy_to(host, &dir, "inbox", false, &cancel, |_| {}).unwrap_err();
    assert!(matches!(e, TransferError::Folder(_)), "{e}");
    // Nothing is left beside the files: no part of a refused or finished copy.
    let mut names: Vec<String> = std::fs::read_dir(home.join("inbox"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["data.bin", "data.bin.d", "link", "named.bin"]);

    // A copy that says more than it sends leaves nothing.
    let (ticket, _) = c
        .copy_to_begin("inbox/short.bin", "short.bin", 10, 0o644, false)
        .unwrap();
    let (r, mut w) = stream::open(&env.socket, &ticket.ticket).unwrap();
    w.write_all(b"12345").unwrap();
    w.finish().unwrap();
    assert!(r.wait_closed().is_err());
    assert!(!home.join("inbox/short.bin").exists());
    assert_eq!(std::fs::read_dir(home.join("inbox")).unwrap().count(), 4);

    // And back: into a folder under its name, or to a name of its own.
    let out = dir.join("out");
    std::fs::create_dir(&out).unwrap();
    let from = |path: &str, dest: &Path, replace: bool| {
        transfer::copy_from(host, path, dest, replace, &cancel, |_| {})
    };
    let (at, size) = from("inbox/named.bin", &out, false).unwrap();
    assert_eq!(
        (at.as_path(), size),
        (out.join("named.bin").as_path(), sent)
    );
    assert!(std::fs::read(&at).unwrap() == data);
    assert_eq!(mode(&at), 0o644);
    let e = from("~/inbox/named.bin", &out, false).unwrap_err();
    assert!(e.to_string().contains("is there already"), "{e}");
    std::fs::write(&at, b"old").unwrap();
    std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o600)).unwrap();
    from("~/inbox/named.bin", &out, true).unwrap();
    assert!(std::fs::read(&at).unwrap() == data);
    assert_eq!(
        mode(&at),
        0o600,
        "a replaced file keeps its own permission bits"
    );
    let (at, _) = from(&abs.display().to_string(), &out.join("other.bin"), false).unwrap();
    assert_eq!(at, out.join("other.bin"));
    let e = from("inbox/none", &out, false).unwrap_err();
    assert!(matches!(e, TransferError::NotFound(_)), "{e}");
    let e = from("inbox", &out, false).unwrap_err();
    assert!(e.to_string().contains("not a regular file"), "{e}");
    assert_eq!(
        std::fs::read_dir(&out).unwrap().count(),
        2,
        "a part was left"
    );
}

/// A copy's part is its own: a file that has the name it would take is neither written into nor
/// removed, and a link put in a part's place while its file comes changes nothing it leads to.
#[test]
fn a_copy_touches_no_file_but_its_own_part() {
    let env = Env::new("transfer-copy-part");
    let home = env.dir.join("home");
    let inbox = home.join("inbox");
    std::fs::create_dir_all(&inbox).unwrap();
    let daemon = env.start_with(&[("HOME", &home)]);
    let (c, _rx) = env.connect();
    let pid = daemon.child.id();

    // The name the daemon's first part would take is another's file.
    let theirs = inbox.join(format!(".thurm-{pid}-0.part"));
    std::fs::write(&theirs, b"theirs").unwrap();
    let put = |name: &str, data: &[u8]| {
        let (ticket, _) = c
            .copy_to_begin("inbox", name, data.len() as u64, 0o644, false)
            .unwrap();
        let (r, mut w) = stream::open(&env.socket, &ticket.ticket).unwrap();
        w.write_all(data).unwrap();
        w.finish().unwrap();
        r.wait_closed()
    };
    put("a.txt", b"first").unwrap();
    assert_eq!(std::fs::read(inbox.join("a.txt")).unwrap(), b"first");
    assert_eq!(std::fs::read(&theirs).unwrap(), b"theirs");
    // A copy that fails leaves it too.
    let (ticket, _) = c.copy_to_begin("inbox", "b.txt", 10, 0o644, false).unwrap();
    let (r, mut w) = stream::open(&env.socket, &ticket.ticket).unwrap();
    w.write_all(b"12345").unwrap();
    w.finish().unwrap();
    assert!(r.wait_closed().is_err());
    assert_eq!(std::fs::read(&theirs).unwrap(), b"theirs");
    std::fs::remove_file(&theirs).unwrap();

    // A private file of the user's, and a copy whose part is swapped for a link to it while
    // its bytes come: the file keeps its bits and its bytes, and nothing is copied.
    let private = home.join("private.key");
    std::fs::write(&private, b"secret").unwrap();
    std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o600)).unwrap();
    let parts = || -> Vec<PathBuf> {
        std::fs::read_dir(&inbox)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "part"))
            .collect()
    };
    let (ticket, _) = c.copy_to_begin("inbox", "c.txt", 10, 0o644, false).unwrap();
    let (r, mut w) = stream::open(&env.socket, &ticket.ticket).unwrap();
    w.write_all(b"12345").unwrap();
    w.flush().unwrap();
    eventually(10, "the copy's part did not appear", || {
        parts()
            .first()
            .is_some_and(|p| std::fs::metadata(p).is_ok_and(|m| m.len() == 5))
    });
    let part = parts().remove(0);
    std::fs::remove_file(&part).unwrap();
    std::os::unix::fs::symlink(&private, &part).unwrap();
    w.write_all(b"67890").unwrap();
    w.finish().unwrap();
    let e = r.wait_closed().unwrap_err();
    assert!(e.to_string().contains("was changed while"), "{e}");
    assert_eq!(
        mode(&private),
        0o600,
        "the link's target got the copy's bits"
    );
    assert_eq!(std::fs::read(&private).unwrap(), b"secret");
    assert!(!inbox.join("c.txt").exists());
    // The link is whoever's put it there: not the daemon's to remove.
    assert!(std::fs::symlink_metadata(&part).unwrap().is_symlink());
}

/// A file larger than a copy takes is not sent by one either; a pane's download of it is as
/// before.
#[test]
fn a_copy_from_the_host_keeps_to_the_size_a_copy_takes() {
    let env = Env::new("transfer-copy-size");
    let home = env.dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let _daemon = env.start_with(&[("HOME", &home)]);
    let (c, _rx) = env.connect();
    // Sparse: its size without its bytes.
    let limit = 4u64 << 30;
    let file = std::fs::File::create(home.join("huge.img")).unwrap();
    file.set_len(limit + 1).unwrap();
    std::fs::File::create(home.join("full.img"))
        .unwrap()
        .set_len(limit)
        .unwrap();

    let e = c.copy_from_begin("huge.img").unwrap_err();
    assert!(e.to_string().contains("too large to send"), "{e}");
    // Refused before any stream was reserved, and a file of just the limit is sent.
    let info = c.copy_from_begin("full.img").unwrap();
    assert_eq!(info.size, limit);
    assert!(info.ticket.is_some());
    let pane = cat_pane(&c, &home);
    let info = c.download_begin(pane, "huge.img", None).unwrap();
    assert_eq!(info.size, limit + 1);
    assert!(info.ticket.is_some());
}
