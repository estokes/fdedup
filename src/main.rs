#[macro_use]
extern crate serde_derive;
use anyhow::{bail, Result};
use fxhash::FxBuildHasher;
//use mapr::Mmap;
use md5::Digest;
use parking_lot::Mutex;
use std::{
    collections::{HashMap, HashSet},
    mem,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    fs::{metadata, read_dir, read_link, File},
    io::AsyncReadExt,
    sync::{OwnedSemaphorePermit, Semaphore},
    task::{self, JoinHandle},
};

const MMAP_LEN: usize = 32384;
const MAX_SYMLINKS: usize = 128;

async fn scan_file<P: AsRef<Path>>(permit: OwnedSemaphorePermit, path: P) -> Result<Digest> {
    let res = {
        let mut ctx = md5::Context::new();
        let mut fd = File::open(path).await?;
        let mut contents = [0u8; 32384];
        loop {
            let n = fd.read(&mut contents[0..]).await?;
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

/*
async fn scan_file_mmap<P: AsRef<Path>>(permit: OwnedSemaphorePermit, path: P) -> Result<Digest> {
    let res = {
        let fd = File::open(path)
            .await?
            .try_into_std()
            .map_err(|_| anyhow::anyhow!("operation in progress"))?;
        task::block_in_place(|| {
            let mmap = unsafe { Mmap::map(&fd)? };
            Ok(md5::compute(&mmap[..]))
        })
    };
    drop(permit);
    res
}

            } else if md.len() <= MMAP_LEN as u64 {
                let path = dirent.path();
                let permit = file_sem.clone().acquire_owned().await?;
                let digest = scan_file(permit, &path).await?;
                res.lock()
                    .entry(digest)
                    .or_insert_with(HashSet::new)
                    .insert(path);
                break;

*/

async fn scan_dir<P: AsRef<Path>>(
    tasks: Arc<Mutex<Vec<JoinHandle<Result<()>>>>>,
    dirs: Arc<Mutex<Vec<PathBuf>>>,
    res: Arc<Mutex<HashMap<Digest, HashSet<PathBuf>, FxBuildHasher>>>,
    dir_sem: Arc<Semaphore>,
    file_sem: Arc<Semaphore>,
    path: P,
) -> Result<()> {
    let permit = dir_sem.acquire_owned().await?;
    let mut dirents = read_dir(path).await?;
    while let Some(dirent) = dirents.next_entry().await? {
        let mut links = 0;
        let mut md = dirent.metadata().await?;
        loop {
            let ft = md.file_type();
            if ft.is_symlink() {
                if links > MAX_SYMLINKS {
                    bail!("too many levels of symbolic links")
                }
                links += 1;
                md = metadata(read_link(dirent.path()).await?).await?;
            } else if ft.is_dir() {
                dirs.lock().push(dirent.path());
                break;
            } else {
                let res = res.clone();
                let permit = file_sem.clone().acquire_owned().await?;
                let path = dirent.path();
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
            }
        }
    }
    drop(permit);
    Ok(())
}

#[derive(Debug, Serialize)]
struct ReportEnt {
    digest: [u8; 16],
    paths: HashSet<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let dir_sem = Arc::new(Semaphore::new(256));
    let file_sem = Arc::new(Semaphore::new(256));
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
            let tasks_ = tasks.clone();
            let dirs = dirs.clone();
            let res = res.clone();
            let file_sem = file_sem.clone();
            let dir_sem = dir_sem.clone();
            tasks.lock().push(task::spawn(async move {
                Ok(scan_dir(tasks_, dirs, res, dir_sem, file_sem, dir).await?)
            }));
        }
        for task in tasks_ {
            task.await??;
        }
    }
    for (digest, paths) in res.lock().drain() {
        if paths.len() > 1 {
            println!(
                "{}",
                serde_json::to_string(&ReportEnt {
                    digest: digest.0,
                    paths: paths
                })?
            );
        }
    }
    Ok(())
}
