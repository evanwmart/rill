//! Int8 vectors in fixed-width Base64URL rows (§5, §6): `n` bytes → a row
//! of `encoded_len(n)` characters and a newline, so row `r` is at byte
//! `r · width` with no index. Cosine over int8 needs no stored scale.

use std::fs::{self, File};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::base64;
use crate::{Result, SHARD, format_err, shard_name};

/// Bytes per row for a `dim`-wide vector: encoded chars plus newline.
pub const fn row_width(dim: usize) -> u64 {
    (base64::encoded_len(dim) + 1) as u64
}

/// Quantise a unit vector: `round(x · 127)` clamped to `[-127, 127]`.
pub fn quantize(v: &[f32]) -> Vec<i8> {
    v.iter().map(|&x| (x * 127.0).round().clamp(-127.0, 127.0) as i8).collect()
}

/// L2-normalise in place; a zero vector stays zero.
pub fn normalize(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        for x in v {
            *x /= n;
        }
    }
}

/// Dot product in i32; the caller divides by norms if it wants cosine.
pub fn dot(a: &[i8], b: &[i8]) -> i32 {
    debug_assert_eq!(a.len(), b.len());
    a.iter().zip(b).map(|(&x, &y)| i32::from(x) * i32::from(y)).sum()
}

/// Squared L2 norm in i32.
pub fn norm_sq(a: &[i8]) -> i32 {
    a.iter().map(|&x| i32::from(x) * i32::from(x)).sum()
}

/// Cosine similarity of two int8 vectors. The quantisation scale cancels.
pub fn cosine(a: &[i8], b: &[i8]) -> f32 {
    let d = dot(a, b) as f32;
    let n = (norm_sq(a) as f32 * norm_sq(b) as f32).sqrt();
    if n == 0.0 { 0.0 } else { d / n }
}

/// Reinterpret int8 as the bytes they are.
fn as_bytes(v: &[i8]) -> &[u8] {
    // SAFETY: i8 and u8 have identical size and alignment; the slice is
    // borrowed for the same lifetime and never written through.
    unsafe { std::slice::from_raw_parts(v.as_ptr().cast::<u8>(), v.len()) }
}

fn from_bytes(b: &[u8]) -> Vec<i8> {
    b.iter().map(|&x| x as i8).collect()
}

/// Writes `NNNN` rows for consecutive ids, rolling every `SHARD`.
pub struct Writer {
    dir: PathBuf,
    dim: usize,
    next: u64,
    shard: Option<BufWriter<File>>,
    line: String,
}

impl Writer {
    pub fn create(dir: impl AsRef<Path>, dim: usize) -> Result<Writer> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        Ok(Writer { dir, dim, next: 0, shard: None, line: String::with_capacity(base64::encoded_len(dim) + 1) })
    }

    pub fn next_id(&self) -> u64 {
        self.next
    }

    pub fn push(&mut self, v: &[i8]) -> Result<()> {
        if v.len() != self.dim {
            return format_err(format!("vector of {} components, shard is {}-wide", v.len(), self.dim));
        }
        let (shard, row) = crate::shard_of(self.next);
        if row == 0 {
            self.finish_shard()?;
            self.shard = Some(BufWriter::new(File::create(self.dir.join(shard_name(shard)))?));
        }
        self.line.clear();
        base64::encode_into(as_bytes(v), &mut self.line);
        self.line.push('\n');
        self.shard.as_mut().expect("shard open").write_all(self.line.as_bytes())?;
        self.next += 1;
        Ok(())
    }

    fn finish_shard(&mut self) -> Result<()> {
        if let Some(mut w) = self.shard.take() {
            w.flush()?;
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<u64> {
        self.finish_shard()?;
        Ok(self.next)
    }
}

/// One row by seek.
pub fn read_row(dir: &Path, shard: u32, row: u64, dim: usize) -> Result<Vec<i8>> {
    let width = row_width(dim);
    let mut f = File::open(dir.join(shard_name(shard)))?;
    f.seek(SeekFrom::Start(row * width))?;
    let mut buf = vec![0u8; width as usize];
    f.read_exact(&mut buf)?;
    decode_row(&buf, dim, shard, row)
}

fn decode_row(buf: &[u8], dim: usize, shard: u32, row: u64) -> Result<Vec<i8>> {
    let (chars, nl) = buf.split_at(buf.len() - 1);
    if nl != b"\n" {
        return format_err(format!("vector shard {shard} row {row}: row not newline-terminated"));
    }
    match base64::decode(chars) {
        Some(bytes) if bytes.len() == dim => Ok(from_bytes(&bytes)),
        _ => format_err(format!("vector shard {shard} row {row}: bad row")),
    }
}

/// A whole shard decoded into one contiguous buffer, for scans.
pub struct Shard {
    pub dim: usize,
    pub rows: u64,
    data: Vec<i8>,
}

impl Shard {
    pub fn read(dir: &Path, shard: u32, dim: usize) -> Result<Shard> {
        let bytes = fs::read(dir.join(shard_name(shard)))?;
        let width = row_width(dim) as usize;
        if bytes.len() % width != 0 {
            return format_err(format!("vector shard {shard}: {} bytes is not a whole number of rows", bytes.len()));
        }
        let rows = (bytes.len() / width) as u64;
        if rows > SHARD {
            return format_err(format!("vector shard {shard}: {rows} rows over SHARD"));
        }
        let mut data = Vec::with_capacity(rows as usize * dim);
        let mut tmp = Vec::with_capacity(dim);
        for (r, chunk) in bytes.chunks_exact(width).enumerate() {
            tmp.clear();
            if chunk[width - 1] != b'\n' || !base64::decode_into(&chunk[..width - 1], &mut tmp) || tmp.len() != dim {
                return format_err(format!("vector shard {shard} row {r}: bad row"));
            }
            data.extend(tmp.iter().map(|&b| b as i8));
        }
        Ok(Shard { dim, rows, data })
    }

    pub fn row(&self, r: u64) -> &[i8] {
        let r = r as usize;
        &self.data[r * self.dim..(r + 1) * self.dim]
    }

    pub fn iter(&self) -> impl Iterator<Item = (u64, &[i8])> {
        self.data.chunks_exact(self.dim).enumerate().map(|(r, v)| (r as u64, v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(seed: u32, dim: usize) -> Vec<f32> {
        let mut v: Vec<f32> = (0..dim).map(|i| (((i as u32 + 1) * (seed + 7)) % 97) as f32 / 48.5 - 1.0).collect();
        normalize(&mut v);
        v
    }

    #[test]
    fn quantised_cosine_tracks_float_cosine() {
        let (a, b) = (unit(1, 384), unit(2, 384));
        let float: f32 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
        let q = cosine(&quantize(&a), &quantize(&b));
        assert!((float - q).abs() < 0.01, "float {float} vs int8 {q}");
        assert!((cosine(&quantize(&a), &quantize(&a)) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn rows_are_fixed_width_and_seekable() {
        assert_eq!(row_width(384), 513);
        assert_eq!(row_width(64), 87);
        let dir = std::env::temp_dir().join(format!("rill-knowledge-vec-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut w = Writer::create(&dir, 64).unwrap();
        let n = SHARD + 3;
        for i in 0..n {
            w.push(&quantize(&unit(i as u32, 64))).unwrap();
        }
        assert_eq!(w.finish().unwrap(), n);
        assert_eq!(fs::metadata(dir.join("0000")).unwrap().len(), SHARD * 87);
        for id in [0, 1, SHARD - 1, SHARD, SHARD + 2] {
            let (s, r) = crate::shard_of(id);
            assert_eq!(read_row(&dir, s, r, 64).unwrap(), quantize(&unit(id as u32, 64)), "row {id}");
        }
        let shard = Shard::read(&dir, 1, 64).unwrap();
        assert_eq!(shard.rows, 3);
        assert_eq!(shard.row(2), quantize(&unit((SHARD + 2) as u32, 64)).as_slice());
        assert_eq!(shard.iter().count(), 3);
        assert!(w_dim_mismatch(&dir));
        fs::remove_dir_all(&dir).unwrap();
    }

    fn w_dim_mismatch(dir: &Path) -> bool {
        let mut w = Writer::create(dir.join("x"), 64).unwrap();
        w.push(&[0i8; 63]).is_err()
    }
}
