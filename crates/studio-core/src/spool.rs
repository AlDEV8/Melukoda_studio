use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs, io,
    path::{Path, PathBuf},
};
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Chunk {
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub payload: Vec<u8>,
    pub sha256: String,
}
impl Chunk {
    pub fn new(sequence: u64, timestamp_ms: u64, payload: Vec<u8>) -> Self {
        let sha256 = format!("{:x}", Sha256::digest(&payload));
        Self {
            sequence,
            timestamp_ms,
            payload,
            sha256,
        }
    }
    pub fn verify(&self) -> bool {
        self.sha256 == format!("{:x}", Sha256::digest(&self.payload))
    }
}
#[derive(Debug)]
pub struct Spool {
    root: PathBuf,
    pub limit_bytes: u64,
}
impl Spool {
    pub fn open(root: impl AsRef<Path>, limit_bytes: u64) -> io::Result<Self> {
        fs::create_dir_all(root.as_ref())?;
        Ok(Self {
            root: root.as_ref().into(),
            limit_bytes,
        })
    }
    fn path(&self, s: u64) -> PathBuf {
        self.root.join(format!("{s:020}.json"))
    }
    pub fn append(&self, c: &Chunk) -> io::Result<()> {
        if !c.verify() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "chunk checksum"));
        };
        if self.used_bytes()? + c.payload.len() as u64 > self.limit_bytes {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "spool limit reached",
            ));
        };
        let final_path = self.path(c.sequence);
        let temp = final_path.with_extension("partial");
        fs::write(&temp, serde_json::to_vec(c).unwrap())?;
        fs::rename(temp, final_path)
    }
    pub fn pending(&self) -> io::Result<Vec<Chunk>> {
        let mut all = vec![];
        for e in fs::read_dir(&self.root)? {
            let p = e?.path();
            if p.extension().and_then(|v| v.to_str()) == Some("json") {
                let c: Chunk = serde_json::from_slice(&fs::read(p)?)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                if c.verify() {
                    all.push(c)
                }
            }
        }
        all.sort_by_key(|c| c.sequence);
        for w in all.windows(2) {
            if w[1].sequence <= w[0].sequence {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unordered spool",
                ));
            }
        }
        Ok(all)
    }
    pub fn acknowledge_through(&self, sequence: u64) -> io::Result<()> {
        for c in self.pending()? {
            if c.sequence <= sequence {
                fs::remove_file(self.path(c.sequence))?
            }
        }
        Ok(())
    }
    pub fn used_bytes(&self) -> io::Result<u64> {
        Ok(self.pending()?.iter().map(|c| c.payload.len() as u64).sum())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    #[test]
    fn atomic_ordered_recovery_and_prune() {
        let d = tempdir().unwrap();
        let s = Spool::open(d.path(), 10).unwrap();
        s.append(&Chunk::new(2, 20, vec![2])).unwrap();
        s.append(&Chunk::new(1, 10, vec![1])).unwrap();
        assert_eq!(s.pending().unwrap()[0].sequence, 1);
        s.acknowledge_through(1).unwrap();
        assert_eq!(s.pending().unwrap()[0].sequence, 2);
    }
    #[test]
    fn enforces_limit() {
        let d = tempdir().unwrap();
        let s = Spool::open(d.path(), 1).unwrap();
        assert!(s.append(&Chunk::new(1, 0, vec![1, 2])).is_err());
    }
}
