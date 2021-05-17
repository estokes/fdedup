use anyhow::Result;
use fxhash::FxBuildHasher;
use mapr::Mmap;
use md5::Digest;
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    mem,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    fs::{read_dir, File},
    io::AsyncReadExt,
    sync::{OwnedSemaphorePermit, Semaphore},
    task::{self, JoinHandle},
};

const MMAP_LEN: usize = 32384;

// for files < MMAP_LEN
async fn scan_file<P: AsRef<Path>>(permit: OwnedSemaphorePermit, path: P) -> Result<Digest> {
    let res = {
        let mut fd = File::open(path).await?;
        let mut contents = [0u8; MMAP_LEN];
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
    };
    drop(permit);
    res
}

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

#[derive(Debug)]
enum MaybeDup {
    Empty,
    Singleton(PathBuf),
    Duplicated(Vec<PathBuf>),
}

impl MaybeDup {
    fn push(&mut self, path: PathBuf) {
        match self {
            MaybeDup::Empty => {
                *self = MaybeDup::Singleton(path);
            }
            MaybeDup::Singleton(path0) => {
                *self = MaybeDup::Duplicated(vec![mem::replace(path0, PathBuf::new()), path]);
            }
            MaybeDup::Duplicated(v) => v.push(path),
        }
    }
}

async fn scan_dir<P: AsRef<Path>>(
    tasks: Arc<Mutex<Vec<JoinHandle<Result<()>>>>>,
    dirs: Arc<Mutex<Vec<PathBuf>>>,
    res: Arc<Mutex<HashMap<Digest, MaybeDup, FxBuildHasher>>>,
    dir_sem: Arc<Semaphore>,
    file_sem: Arc<Semaphore>,
    path: P,
) -> Result<()> {
    let permit = dir_sem.acquire_owned().await?;
    let mut dirents = read_dir(path).await?;
    while let Some(dirent) = dirents.next_entry().await? {
        let md = dirent.metadata().await?;
        let ft = dirent.file_type().await?;
        if ft.is_symlink() {
            continue; // skip it
        } else if ft.is_dir() {
            let path = dirent.path();
            dirs.lock().push(path);
        } else if md.len() <= MMAP_LEN as u64 {
            let path = dirent.path();
            let permit = file_sem.clone().acquire_owned().await?;
            let digest = scan_file(permit, &path).await?;
            res.lock()
                .entry(digest)
                .or_insert(MaybeDup::Empty)
                .push(path);
        } else {
            let res = res.clone();
            let permit = file_sem.clone().acquire_owned().await?;
            let path = dirent.path();
            tasks.lock().push(task::spawn(async move {
                let digest = scan_file_mmap(permit, &path).await?;
                res.lock().entry(digest).or_insert(MaybeDup::Empty).push(path);
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
    for (digest, paths) in res.lock().iter() {
        match paths {
            MaybeDup::Empty | MaybeDup::Singleton(_) => (),
            MaybeDup::Duplicated(paths) => {
                println!("digest: {:?}, paths: {:?}", digest, paths);
            }
        }
    }
    Ok(())
}
