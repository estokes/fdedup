use anyhow::Result;
use fxhash::FxBuildHasher;
use mapr::Mmap;
use md5::Digest;
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    fs::{read_dir, File},
    io::AsyncReadExt,
    sync::{OwnedSemaphorePermit, Semaphore, SemaphorePermit},
    task::{self, JoinHandle},
};

async fn scan_file<P: AsRef<Path>>(permit: OwnedSemaphorePermit, path: P) -> Result<Digest> {
    const MMAP_LEN: u64 = 32384; // mmap files bigger than 32k
    let res = {
        let mut fd = File::open(path).await?;
        let md = fd.metadata().await?;
        if md.len() <= MMAP_LEN {
            let mut contents = [0u8; 32384];
            let mut pos: usize = 0;
            loop {
                let n = fd.read(&mut contents[pos..]).await?;
                if n > 0 {
                    pos += n;
                } else {
                    break;
                }
            }
            Ok(md5::compute(&contents[0..pos]))
        } else {
            task::block_in_place(|| {
                let fd = fd
                    .try_into_std()
                    .map_err(|_| anyhow::anyhow!("operation in progress"))?;
                let mmap = unsafe { Mmap::map(&fd)? };
                Ok(md5::compute(&mmap[..]))
            })
        }
    };
    drop(permit);
    res
}

async fn scan_dir<P: AsRef<Path>>(
    tasks: Arc<Mutex<Vec<JoinHandle<Result<()>>>>>,
    dirs: Arc<Mutex<Vec<PathBuf>>>,
    res: Arc<Mutex<HashMap<Digest, Vec<PathBuf>, FxBuildHasher>>>,
    dir_sem: Arc<Semaphore>,
    file_sem: Arc<Semaphore>,
    path: P,
) -> Result<()> {
    let permit = dir_sem.acquire_owned().await?;
    let mut dirents = read_dir(path).await?;
    while let Some(dirent) = dirents.next_entry().await? {
        let ft = dirent.file_type().await?;
        if ft.is_symlink() {
            continue; // skip it
        } else if ft.is_dir() {
            dirs.lock().push(dirent.path());
        } else {
            let res = res.clone();
            let permit = file_sem.clone().acquire_owned().await?;
            let path = dirent.path();
            tasks.lock().push(task::spawn(async move {
                let digest = scan_file(permit, &path).await?;
                res.lock().entry(digest).or_insert_with(Vec::new).push(path);
                Ok(())
            }));
        }
    }
    drop(permit);
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let dir_sem = Arc::new(Semaphore::new(256));
    let file_sem = Arc::new(Semaphore::new(512));
    let tasks = Arc::new(Mutex::new(vec![]));
    let dirs = Arc::new(Mutex::new(vec![]));
    let res = Arc::new(Mutex::new(HashMap::with_hasher(FxBuildHasher::default())));
    scan_dir(
        tasks.clone(),
        dirs.clone(),
        res.clone(),
        dir_sem.clone(),
        file_sem.clone(),
        ".",
    )
    .await?;
    loop {
        if let Some(dir) = dirs.lock().pop() {
            let tasks_ = tasks.clone();
            let dirs = dirs.clone();
            let res = res.clone();
            let file_sem = file_sem.clone();
            let dir_sem = dir_sem.clone();
            tasks.lock().push(task::spawn(async move {
                Ok(scan_dir(tasks_, dirs, res, dir_sem, file_sem, dir).await?)
            }));
        } else if let Some(jh) = tasks.lock().pop() {
            jh.await??
        } else {
            break
        }
    }
    for (digest, paths) in res.lock().iter() {
        if paths.len() > 1 {
            println!("digest: {:?}, paths: {:?}", digest, paths);
        }
    }
    Ok(())
}
