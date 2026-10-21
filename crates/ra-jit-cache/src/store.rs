//! Where results are kept: in memory for the run, and in files for the runs after it.
//!
//! An entry is a file named by a hash of the code and the arguments, holding both, so that two
//! keys of one hash are told apart by what is in the file. It is written to a file of its own
//! and renamed into place, so a reader finds a whole entry or none, whoever stops writing
//! when. When the directory holds more than its size, the entries read or written least
//! recently go first: a hit sets the time of its file.

use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};

use serde::{Serialize, de::DeserializeOwned};

use crate::__private::{Code, decode, encode};

/// What an entry's file begins with; another format is another name.
const MAGIC: &[u8; 16] = b"ra-jit-fn-cache1";

/// How much is kept on disk unless `RA_JIT_FN_CACHE_SIZE` says.
const DEFAULT_SIZE: u64 = 512 << 20;

/// How much the memory of a run holds before it starts over.
const MEMORY_SIZE: usize = 256 << 20;

/// What a call is looked up by.
pub struct Key {
    code: [u64; 2],
    /// The function, for what `RA_JIT_FN_CACHE_LOG` prints.
    name: &'static str,
    args: Vec<u8>,
}

/// The key of a call of the function whose code is `code` with `args`, or `None` when the call
/// is to run as written: ra-jit did not compile the caller, refused to cache the function, or
/// caching is off.
pub fn key<A: Serialize>(code: &Code, args: &A) -> Option<Key> {
    let cache = Cache::get()?;
    if !code.known() {
        let why = code.text();
        if !why.is_empty() && cache.lock().refusals.insert(why) {
            eprintln!("ra-jit: {why}");
        }
        return None;
    }
    let name = code.text();
    let Some(args) = encode(args) else {
        cache.log(name, "not cached: the arguments could not be written");
        return None;
    };
    let mut memory = cache.lock();
    // A live edit gave the function other code: what the old code computed is never asked
    // for again.
    if let Some(was) = memory.sites.insert(name, code.hash)
        && was != code.hash
        && let Some(dropped) = memory.results.remove(&was)
    {
        memory.bytes -= dropped.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>();
        drop(memory);
        cache.log(
            name,
            &format!("the code changed: {} results of the old code dropped", dropped.len()),
        );
    }
    Some(Key { code: code.hash, name, args })
}

/// What `body` returns for `key`: from memory, from disk, or computed and kept.
pub fn call<R: Serialize + DeserializeOwned>(key: Key, body: impl FnOnce() -> R) -> R {
    let Some(cache) = Cache::get() else { return body() };
    let found = cache.lock().results.get(&key.code).and_then(|it| it.get(&key.args)).cloned();
    if let Some(bytes) = found
        && let Some(value) = decode(&bytes)
    {
        cache.log(key.name, "found in memory");
        return value;
    }
    if let Some(disk) = &cache.disk
        && let Some(bytes) = disk.get(&key)
    {
        if let Some(value) = decode(&bytes) {
            cache.log(key.name, "found on disk");
            cache.remember(key, bytes);
            return value;
        }
        disk.remove(&key);
    }
    let value = body();
    let Some(bytes) = encode(&value) else {
        cache.log(key.name, "computed, not kept: the result could not be written");
        return value;
    };
    match &cache.disk {
        Some(disk) => match disk.put(&key, &bytes) {
            Ok(()) => cache.log(key.name, "computed and kept"),
            Err(e) => cache.log(key.name, &format!("computed, kept in memory: {e}")),
        },
        None => cache.log(key.name, "computed, kept in memory"),
    }
    cache.remember(key, bytes);
    value
}

struct Cache {
    disk: Option<Disk>,
    log: bool,
    memory: Mutex<Memory>,
}

#[derive(Default)]
struct Memory {
    /// Results by the code and then the arguments.
    results: HashMap<[u64; 2], HashMap<Vec<u8>, Vec<u8>>>,
    bytes: usize,
    /// The code each function had at its last call.
    sites: HashMap<&'static str, [u64; 2]>,
    /// The reasons already given for not caching a function.
    refusals: HashSet<&'static str>,
}

impl Cache {
    fn get() -> Option<&'static Cache> {
        static CACHE: OnceLock<Option<Cache>> = OnceLock::new();
        CACHE.get_or_init(Cache::new).as_ref()
    }

    fn new() -> Option<Cache> {
        let mode = std::env::var("RA_JIT_FN_CACHE").unwrap_or_default();
        if matches!(mode.as_str(), "off" | "0" | "false" | "no") {
            return None;
        }
        let disk = (mode != "memory").then(Disk::find).flatten();
        let log = std::env::var_os("RA_JIT_FN_CACHE_LOG").is_some_and(|it| it != "0");
        Some(Cache { disk, log, memory: Mutex::default() })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Memory> {
        self.memory.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn log(&self, name: &str, what: &str) {
        if self.log {
            eprintln!("ra-jit cache: {name}: {what}");
        }
    }

    fn remember(&self, key: Key, result: Vec<u8>) {
        let mut memory = self.lock();
        let size = key.args.len() + result.len();
        if memory.bytes + size > MEMORY_SIZE {
            memory.results.clear();
            memory.bytes = 0;
        }
        memory.bytes += size;
        memory.results.entry(key.code).or_default().insert(key.args, result);
    }
}

struct Disk {
    dir: PathBuf,
    limit: u64,
    /// What the directory holds, once a write has looked.
    size: Mutex<Option<u64>>,
    written: AtomicU64,
}

impl Disk {
    /// `RA_JIT_FN_CACHE_DIR`, or `ra-jit/fn-cache` in the target directory the program is in:
    /// the first directory above it with cargo's `CACHEDIR.TAG`. A program elsewhere keeps
    /// its results for the run alone.
    fn find() -> Option<Disk> {
        let dir = match std::env::var_os("RA_JIT_FN_CACHE_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => {
                let exe = std::env::current_exe().ok()?;
                let target =
                    exe.ancestors().skip(1).find(|it| it.join("CACHEDIR.TAG").is_file())?;
                target.join("ra-jit").join("fn-cache")
            }
        };
        let limit = std::env::var("RA_JIT_FN_CACHE_SIZE")
            .ok()
            .and_then(|it| parse_size(&it))
            .unwrap_or(DEFAULT_SIZE);
        Some(Disk { dir, limit, size: Mutex::new(None), written: AtomicU64::new(0) })
    }

    fn path(&self, key: &Key) -> PathBuf {
        let hash = digest(key);
        let name = format!("{hash:032x}");
        self.dir.join(&name[..2]).join(&name[2..])
    }

    fn get(&self, key: &Key) -> Option<Vec<u8>> {
        let path = self.path(key);
        let bytes = fs::read(&path).ok()?;
        let result = parse(&bytes, key)?.to_vec();
        // Least recently used goes first: this one was used now.
        if let Ok(file) = fs::File::options().write(true).open(&path) {
            let _ = file.set_modified(SystemTime::now());
        }
        Some(result)
    }

    fn remove(&self, key: &Key) {
        let _ = fs::remove_file(self.path(key));
    }

    fn put(&self, key: &Key, result: &[u8]) -> std::io::Result<()> {
        let path = self.path(key);
        let parent = path.parent().expect("an entry is in a directory");
        fs::create_dir_all(parent)?;
        let temporary = self.dir.join("tmp");
        fs::create_dir_all(&temporary)?;
        let n = self.written.fetch_add(1, Ordering::Relaxed);
        let temporary = temporary.join(format!("{}.{n}", std::process::id()));
        let mut bytes = Vec::with_capacity(MAGIC.len() + 24 + key.args.len() + result.len());
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&key.code[0].to_le_bytes());
        bytes.extend_from_slice(&key.code[1].to_le_bytes());
        bytes.extend_from_slice(&(key.args.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&key.args);
        bytes.extend_from_slice(result);
        let written = fs::File::create(&temporary)
            .and_then(|mut file| file.write_all(&bytes))
            .and_then(|()| fs::rename(&temporary, &path));
        if written.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        written?;
        self.grew(bytes.len() as u64);
        Ok(())
    }

    /// Notes that an entry of `added` bytes was written, and makes room when the directory
    /// holds more than its limit.
    fn grew(&self, added: u64) {
        let mut size = self.size.lock().unwrap_or_else(|e| e.into_inner());
        let now = match *size {
            Some(was) => was + added,
            None => entries(&self.dir).iter().map(|it| it.2).sum(),
        };
        *size = Some(if now > self.limit { self.evict() } else { now });
    }

    /// Takes away the entries used least recently until a quarter of the limit is free, and
    /// what an interrupted write left. Gives what is left.
    fn evict(&self) -> u64 {
        let mut all = entries(&self.dir);
        all.sort_by_key(|it| it.1);
        let mut size: u64 = all.iter().map(|it| it.2).sum();
        let keep = self.limit / 4 * 3;
        for (path, _, len) in all {
            if size <= keep {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                size -= len;
            }
        }
        let hour_ago = SystemTime::now() - Duration::from_secs(3600);
        for left in fs::read_dir(self.dir.join("tmp")).into_iter().flatten().flatten() {
            let old = left.metadata().and_then(|it| it.modified()).is_ok_and(|it| it < hour_ago);
            if old {
                let _ = fs::remove_file(left.path());
            }
        }
        size
    }
}

/// The result in the bytes of an entry, when the entry is of `key`.
fn parse<'a>(bytes: &'a [u8], key: &Key) -> Option<&'a [u8]> {
    let rest = bytes.strip_prefix(MAGIC)?;
    let (code, rest) = rest.split_at_checked(16)?;
    let (len, rest) = rest.split_at_checked(8)?;
    let len = usize::try_from(u64::from_le_bytes(len.try_into().ok()?)).ok()?;
    let (args, result) = rest.split_at_checked(len)?;
    let same_code =
        code[..8] == key.code[0].to_le_bytes() && code[8..] == key.code[1].to_le_bytes();
    (same_code && args == key.args).then_some(result)
}

/// Every entry under `dir`, with when it was last used and its size.
fn entries(dir: &Path) -> Vec<(PathBuf, SystemTime, u64)> {
    let mut found = Vec::new();
    for shard in fs::read_dir(dir).into_iter().flatten().flatten() {
        if shard.file_name() == "tmp" {
            continue;
        }
        for entry in fs::read_dir(shard.path()).into_iter().flatten().flatten() {
            if let Ok(meta) = entry.metadata()
                && meta.is_file()
            {
                let used = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                found.push((entry.path(), used, meta.len()));
            }
        }
    }
    found
}

/// FNV-1a of 128 bits over the code and the arguments: the same in every build, and only a
/// name, as an entry holds what it is of.
fn digest(key: &Key) -> u128 {
    const PRIME: u128 = 0x0000000001000000000000000000013B;
    let mut hash: u128 = 0x6c62272e07bb014262b821756295c58d;
    let code = key.code.iter().flat_map(|it| it.to_le_bytes());
    for byte in code.chain(key.args.iter().copied()) {
        hash ^= u128::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let (number, shift) = match text.as_bytes().last()? {
        b'K' | b'k' => (&text[..text.len() - 1], 10),
        b'M' | b'm' => (&text[..text.len() - 1], 20),
        b'G' | b'g' => (&text[..text.len() - 1], 30),
        _ => (text, 0),
    };
    number.trim().parse::<u64>().ok()?.checked_mul(1 << shift)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(args: &[u8]) -> Key {
        Key { code: [1, 2], name: "f", args: args.to_vec() }
    }

    fn disk(limit: u64) -> (Disk, PathBuf) {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("ra-jit-fn-cache-test-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        (Disk { dir: dir.clone(), limit, size: Mutex::new(None), written: AtomicU64::new(0) }, dir)
    }

    #[test]
    fn an_entry_is_found_by_its_key_alone() {
        let (disk, dir) = disk(1 << 20);
        disk.put(&key(b"a"), b"result").unwrap();
        assert_eq!(disk.get(&key(b"a")).as_deref(), Some(&b"result"[..]));
        assert_eq!(disk.get(&key(b"b")), None);
        let other = Key { code: [1, 3], ..key(b"a") };
        assert_eq!(disk.get(&other), None);
        // An entry under the name of another key is not that key's.
        fs::create_dir_all(disk.path(&key(b"b")).parent().unwrap()).unwrap();
        fs::copy(disk.path(&key(b"a")), disk.path(&key(b"b"))).unwrap();
        assert_eq!(disk.get(&key(b"b")), None);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_torn_entry_is_none() {
        let (disk, dir) = disk(1 << 20);
        disk.put(&key(b"a"), b"result").unwrap();
        let path = disk.path(&key(b"a"));
        let bytes = fs::read(&path).unwrap();
        fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();
        assert!(disk.get(&key(b"a")).is_none_or(|it| it != b"result"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn the_least_recently_used_go_first() {
        let (disk, dir) = disk(4096);
        let big = vec![7u8; 1000];
        for i in 0..3u8 {
            disk.put(&key(&[i]), &big).unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
        // The first is used again, so the second is the oldest.
        assert!(disk.get(&key(&[0])).is_some());
        std::thread::sleep(Duration::from_millis(20));
        disk.put(&key(&[3]), &big).unwrap();
        disk.put(&key(&[4]), &big).unwrap();
        assert!(disk.get(&key(&[1])).is_none(), "the oldest went");
        assert!(disk.get(&key(&[4])).is_some(), "the newest stayed");
        assert!(entries(&dir).iter().map(|it| it.2).sum::<u64>() <= 4096);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("512M"), Some(512 << 20));
        assert_eq!(parse_size("10"), Some(10));
        assert_eq!(parse_size("1g"), Some(1 << 30));
        assert_eq!(parse_size("x"), None);
    }
}
