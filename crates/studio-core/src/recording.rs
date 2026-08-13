use std::{
    fs::{self, File},
    io::{self, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};
/// Crash-tolerant PCM WAV archive writer. It writes a valid header on clean close;
/// a `.partial` can be recovered by `recover_wav` after an unexpected stop.
pub struct WavRecorder {
    final_path: PathBuf,
    partial_path: PathBuf,
    file: File,
    bytes: u32,
}
impl WavRecorder {
    pub fn start(path: impl AsRef<Path>) -> io::Result<Self> {
        let final_path = path.as_ref().to_path_buf();
        let partial_path = final_path.with_extension("wav.partial");
        let mut file = File::create(&partial_path)?;
        file.write_all(&wav_header(0))?;
        Ok(Self {
            final_path,
            partial_path,
            file,
            bytes: 0,
        })
    }
    pub fn write_i16(&mut self, samples: &[i16]) -> io::Result<()> {
        let mut bytes = Vec::with_capacity(samples.len() * 2);
        for s in samples {
            bytes.extend(s.to_le_bytes())
        }
        self.file.write_all(&bytes)?;
        self.bytes += bytes.len() as u32;
        Ok(())
    }
    pub fn finish(mut self) -> io::Result<PathBuf> {
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&wav_header(self.bytes))?;
        self.file.sync_all()?;
        fs::rename(&self.partial_path, &self.final_path)?;
        Ok(self.final_path)
    }
}
fn wav_header(data: u32) -> [u8; 44] {
    let mut h = [0; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data).to_le_bytes());
    h[8..16].copy_from_slice(b"WAVEfmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes());
    h[22..24].copy_from_slice(&2u16.to_le_bytes());
    h[24..28].copy_from_slice(&48_000u32.to_le_bytes());
    h[28..32].copy_from_slice(&192_000u32.to_le_bytes());
    h[32..34].copy_from_slice(&4u16.to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data.to_le_bytes());
    h
}
pub fn recover_wav(partial: impl AsRef<Path>) -> io::Result<()> {
    let p = partial.as_ref();
    let len = fs::metadata(p)?.len();
    if len < 44 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "WAV is too short",
        ));
    }
    let data = (len - 44) as u32;
    let mut f = File::options().write(true).open(p)?;
    f.seek(SeekFrom::Start(0))?;
    f.write_all(&wav_header(data))?;
    f.sync_all()
}
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    #[test]
    fn produces_playable_wav() {
        let d = tempdir().unwrap();
        let p = d.path().join("archive.wav");
        let mut r = WavRecorder::start(&p).unwrap();
        r.write_i16(&[0, 0, 1, -1]).unwrap();
        let final_path = r.finish().unwrap();
        let b = fs::read(final_path).unwrap();
        assert_eq!(&b[0..4], b"RIFF");
        assert_eq!(b.len(), 52);
    }
}
