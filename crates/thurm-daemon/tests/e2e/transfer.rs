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
    let dir = env.dir.join("drops").join(pane.to_string());
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
    assert_eq!(mode(&env.dir.join("drops")), 0o700);
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
        .filter(|n| n != ".parts")
        .collect();
    got.sort();
    assert_eq!(got, ["[2J", "evil-2.txt", "evil.txt"]);
}

#[test]
fn an_upload_that_would_fill_the_disk_is_refused() {
    let env = Env::new("transfer-full");
    let _daemon = env.start_with(&[("THURM_TRANSFER_FREE_BYTES", Path::new("1073741900"))]);
    let (c, _rx) = env.connect();
    let dir = work_dir(&env);
    let pane = cat_pane(&c, &dir);
    // 1 GiB must stay free: 76 bytes fit, 77 don't.
    c.upload_begin(pane, "fits", 76, None).unwrap();
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
    std::fs::write(&file, &data).unwrap();
    let old = std::time::SystemTime::now() - Duration::from_secs(60);
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(old)
        .unwrap();
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
    assert!(e.to_string().contains("cannot open"), "{e}");
}
