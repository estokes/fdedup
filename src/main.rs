#[macro_use]
extern crate serde_derive;
use anyhow::{Context, Result};
use fxhash::FxBuildHasher;
use md5::Digest;
use parking_lot::Mutex;
use std::{
    collections::{HashMap, HashSet},
    mem,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    fs::{canonicalize, metadata, read_dir, read_link, File},
    io::AsyncReadExt,
    sync::{OwnedSemaphorePermit, Semaphore},
    task::{self, JoinHandle},
    time,
};

static PROGRESS: Duration = Duration::from_secs(120);
const BUF: usize = 32384;
const MAX_SYMLINKS: usize = 128;

async fn scan_file<P: AsRef<Path>>(permit: OwnedSemaphorePermit, path: P) -> Result<Digest> {
    let res = {
        let mut ctx = md5::Context::new();
        let mut fd = time::timeout(PROGRESS, File::open(path.as_ref()))
            .await
            .with_context(|| format!("timeout opening file {:?}", path.as_ref()))?
            .with_context(|| format!("error opening file {:?}", path.as_ref()))?;
        let mut contents = [0u8; BUF];
        loop {
            let n = time::timeout(PROGRESS, fd.read(&mut contents[0..]))
                .await
                .with_context(|| format!("timeout reading file {:?}", path.as_ref()))?
                .with_context(|| format!("error reading file {:?}", path.as_ref()))?;
            if n > 0 {
                ctx.consume(&contents[0..n])
            } else {
                break;
            }
        }
        Ok(ctx.compute())
    };
    drop(permit);
    res
}

async fn scan_dir<P: AsRef<Path>>(
    tasks: Arc<Mutex<Vec<JoinHandle<Result<()>>>>>,
    dirs: Arc<Mutex<Vec<PathBuf>>>,
    res: Arc<Mutex<HashMap<Digest, HashSet<PathBuf>, FxBuildHasher>>>,
    dir_sem: Arc<Semaphore>,
    file_sem: Arc<Semaphore>,
    path: P,
) -> Result<()> {
    let permit = dir_sem.acquire_owned().await?;
    let mut dirents = read_dir(path.as_ref())
        .await
        .with_context(|| format!("reading directory {:?}", path.as_ref()))?;
    while let Some(dirent) = dirents
        .next_entry()
        .await
        .with_context(|| format!("reading directory {:?}", path.as_ref()))?
    {
        let mut links = 0;
        let path = dirent.path();
        let mut md = dirent
            .metadata()
            .await
            .with_context(|| format!("getting metadata for {:?}", path))?;
        loop {
            let ft = md.file_type();
            if ft.is_symlink() {
                if links > MAX_SYMLINKS {
                    eprintln!(
                        "too many levels of symbolic links following {:?}, skipping",
                        path
                    );
                    break;
                }
                links += 1;
                let target = read_link(&path)
                    .await
                    .with_context(|| format!("reading symbolic link {:?}", path))?;
                match metadata(&target).await {
                    Ok(dat) => {
                        md = dat;
                    }
                    Err(e) => {
                        eprintln!(
                            "WARNING! skipping broken symlink {:?} target {:?}, {}",
                            path, target, e
                        );
                        break;
                    }
                }
            } else if ft.is_dir() {
                let path = canonicalize(&path)
                    .await
                    .with_context(|| format!("getting canonical path of dir {:?}", path))?;
                dirs.lock().push(path);
                break;
            } else if md.len() == 0 {
                eprintln!("skipping empty file {:?}", path);
                break;
            } else if ft.is_file() {
                let res = res.clone();
                let permit = file_sem.clone().acquire_owned().await?;
                let task = task::spawn(async move {
                    let digest = scan_file(permit, &path).await?;
                    res.lock()
                        .entry(digest)
                        .or_insert_with(HashSet::new)
                        .insert(path);
                    Ok(())
                });
                tasks.lock().push(task);
                break;
            } else {
                eprintln!("skipping non regular file {:?}", path);
                break;
            }
        }
    }
    drop(permit);
    Ok(())
}

#[derive(Debug, Serialize)]
struct Duplicate {
    digest: [u8; 16],
    paths: HashSet<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut checked = HashSet::new();
    let dir_sem = Arc::new(Semaphore::new(256));
    let file_sem = Arc::new(Semaphore::new(512));
    let tasks = Arc::new(Mutex::new(vec![]));
    let dirs = Arc::new(Mutex::new(vec![]));
    let res = Arc::new(Mutex::new(HashMap::with_hasher(FxBuildHasher::default())));
    dirs.lock().push(PathBuf::from("."));
    let mut work = true;
    while work {
        let dirs_ = mem::replace(&mut *dirs.lock(), Vec::new());
        let tasks_ = mem::replace(&mut *tasks.lock(), Vec::new());
        work = dirs_.len() > 0 || tasks_.len() > 0;
        for dir in dirs_ {
            if checked.contains(&dir) {
                eprintln!("skipping already checked directory {:?}", dir)
            } else {
                checked.insert(dir.clone());
                let tasks_ = tasks.clone();
                let dirs = dirs.clone();
                let res = res.clone();
                let file_sem = file_sem.clone();
                let dir_sem = dir_sem.clone();
                tasks.lock().push(task::spawn(async move {
                    Ok(scan_dir(tasks_, dirs, res, dir_sem, file_sem, &dir)
                        .await
                        .with_context(|| format!("scanning directory {:?}", dir))?)
                }));
            }
        }
        for task in tasks_ {
            match task.await {
                Err(e) => eprintln!("internal error awaiting task {}", e),
                Ok(Err(e)) => eprintln!("WARNING! {}", e),
                Ok(Ok(())) => (),
            }
        }
    }
    for (digest, paths) in res.lock().drain() {
        if paths.len() > 1 {
            println!(
                "{}",
                serde_json::to_string(&Duplicate {
                    digest: digest.0,
                    paths: paths
                })?
            );
        }
    }
    Ok(())
}
