use std::{
    collections::BTreeMap,
    io::{Read, Seek},
    os::unix::fs::MetadataExt,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{bail, Result};
use serde::Deserialize;
use slog::{error, info, warn, Logger};

#[derive(Clone)]
pub struct SdrTail(Arc<Inner>);

#[derive(Clone, Deserialize)]
#[allow(unused)]
pub struct RecordBase {
    time: String,
    model: String,
}

#[derive(Clone, Deserialize, Debug)]
#[allow(unused)]
#[allow(non_snake_case)]
pub struct RecordAcuriteTower {
    time: String,
    model: String,
    id: u64,
    channel: String,
    pub battery_ok: i64,
    pub temperature_C: f32,
    pub humidity: f32,
    mic: String,
}

fn parse(buf: &[u8]) -> Result<Option<RecordAcuriteTower>> {
    let rb: RecordBase = serde_json::from_slice(buf)?;
    if rb.model != "Acurite-Tower" {
        /*
         * We don't currently know what to do with other types of devices.
         */
        return Ok(None);
    }

    Ok(Some(serde_json::from_slice(buf)?))
}

impl SdrTail {
    pub fn new(log: Logger, file: PathBuf) -> Result<SdrTail> {
        let sdr = SdrTail(Arc::new(Inner {
            log,
            file,
            locked: Mutex::new(Locked { current: Default::default() }),
        }));

        let sdr0 = sdr.clone();
        std::thread::Builder::new()
            .name("sdrtail".into())
            .spawn(|| sdrtail_thread_noerr(sdr0))?;

        Ok(sdr)
    }

    pub fn values(&self) -> Vec<(String, RecordAcuriteTower)> {
        self.0
            .locked
            .lock()
            .unwrap()
            .current
            .iter()
            .map(|(a, b)| (a.clone(), b.clone()))
            .collect()
    }
}

struct Inner {
    log: Logger,
    file: PathBuf,
    locked: Mutex<Locked>,
}

struct Locked {
    current: BTreeMap<String, RecordAcuriteTower>,
}

fn sdrtail_thread_noerr(sdr: SdrTail) {
    let log = &sdr.0.log;

    loop {
        if let Err(e) = sdrtail_thread(&sdr) {
            error!(log, "sdrtail error: {e}");
        }

        std::thread::sleep(Duration::from_secs(2));
    }
}

fn sdrtail_thread(sdr: &SdrTail) -> Result<()> {
    let i = &sdr.0;
    let log = &i.log;

    /*
     * Attempt to open the file.
     */
    let (mut f, md) = match std::fs::File::open(&i.file) {
        Ok(f) => {
            let md = f.metadata()?;
            (f, md)
        }
        Err(e) => bail!("open {:?}: {e}", i.file),
    };

    /*
     * Store the original device/inode numbers so that we can tell if the file
     * has been replaced.
     */
    let dev = md.dev();
    let ino = md.ino();
    info!(log, "path {:?} has dev {dev:X} inode {ino:X}", i.file);

    let mut pos = if md.len() > 16 * 1024 {
        /*
         * Seek to within 16K of the end of the file.
         */
        let pos = md.len().checked_sub(16 * 1024).unwrap();
        info!(log, "file size is {}, picking up at {pos}", md.len());
        pos
    } else {
        info!(log, "file size is {}, starting at beginning", md.len());
        0
    };

    f.seek(std::io::SeekFrom::Start(pos))?;

    /*
     * Now, read data until we hit EOF, splitting it into lines to process.
     */
    let mut s = Vec::new();
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let sz = f.read(&mut buf)?;
        pos = pos.checked_add(sz.try_into().unwrap()).unwrap();

        if sz == 0 {
            /*
             * This is EOF, but the SDR data logger should continue to write
             * more soon.  Take this opportunity to confirm that the file
             * has not changed.
             */
            if let Ok(md) = std::fs::metadata(&i.file) {
                let mut new_file = false;
                if md.dev() != dev {
                    info!(
                        log,
                        "file {:?}: changed dev {dev:X} -> {:X}",
                        i.file,
                        md.dev(),
                    );
                    new_file = true;
                }
                if md.ino() != ino {
                    info!(
                        log,
                        "file {:?}: changed ino {ino:X} -> {:X}",
                        i.file,
                        md.ino(),
                    );
                    new_file = true;
                }
                if pos > md.size() {
                    /*
                     * If the file has been truncated in place, we need to start
                     * at the top.
                     */
                    info!(log, "file {:?}: shrunk!", i.file);
                    new_file = true;
                }
                if new_file {
                    info!(log, "reopening file {:?}", i.file);
                    return Ok(());
                }
            }

            /*
             * Wait and try again!  We could use some kind of file event
             * notification but ... I am already in my pyjamas.
             */
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }

        for b in &buf[0..sz] {
            if *b == b'\n' {
                /*
                 * Process whatever we have in the accumulator...
                 */
                match parse(&s) {
                    Ok(Some(r)) => {
                        let mut l = i.locked.lock().unwrap();
                        let id = format!(
                            "{}-{:08}-{}",
                            r.model.to_lowercase(),
                            r.id,
                            r.channel.to_lowercase()
                        );
                        l.current.insert(id, r);
                    }
                    Ok(None) => (),
                    Err(e) => warn!(log, "file {:?} parse error: {e}", i.file),
                }

                s.clear();
            } else {
                s.push(*b);
            }
        }
    }
}
