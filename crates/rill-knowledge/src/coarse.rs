//! CountSketch projection (§6): `DIM → COARSE_DIM` with no stored matrix.
//! Each source component lands in one bucket with a sign, both from
//! `splitmix64(seed ^ (i · φ))`; the seed in the manifest is the contract.

use crate::{COARSE_DIM, DIM, vector};

const PHI: u64 = 0x9E37_79B9_7F4A_7C15;

/// The standard SplitMix64 finaliser.
pub fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(PHI);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Bucket and sign for source component `i`.
pub fn slot(seed: u64, i: usize) -> (usize, f32) {
    let h = splitmix64(seed ^ (i as u64).wrapping_mul(PHI));
    let bucket = (h % COARSE_DIM as u64) as usize;
    let sign = if (h >> 32) & 1 == 0 { 1.0 } else { -1.0 };
    (bucket, sign)
}

/// A projector for one seed: the slots, precomputed once.
pub struct Projector {
    slots: Vec<(usize, f32)>,
}

impl Projector {
    pub fn new(seed: u64) -> Projector {
        Projector { slots: (0..DIM).map(|i| slot(seed, i)).collect() }
    }

    /// Project a full f32 vector to a unit coarse vector.
    pub fn project(&self, full: &[f32]) -> Vec<f32> {
        debug_assert_eq!(full.len(), DIM);
        let mut out = vec![0.0f32; COARSE_DIM];
        for (x, &(bucket, sign)) in full.iter().zip(&self.slots) {
            out[bucket] += sign * x;
        }
        vector::normalize(&mut out);
        out
    }

    /// Project and quantise: what the coarse shards store.
    pub fn project_i8(&self, full: &[f32]) -> Vec<i8> {
        vector::quantize(&self.project(full))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitmix_matches_the_reference_sequence() {
        // First outputs of SplitMix64 seeded with 0, per the reference
        // implementation (Steele, Lea, Flood 2014).
        assert_eq!(splitmix64(0), 0xE220_A839_7B1D_CDAF);
        assert_eq!(splitmix64(PHI), 0x6E78_9E6A_A1B9_65F4);
    }

    #[test]
    fn projection_is_deterministic_and_preserves_neighbourhood() {
        let p = Projector::new(0x7269_6c6c);
        let q = Projector::new(0x7269_6c6c);
        let mut a: Vec<f32> = (0..DIM).map(|i| ((i * 31) % 17) as f32 - 8.0).collect();
        vector::normalize(&mut a);
        assert_eq!(p.project(&a), q.project(&a));
        // A near neighbour stays nearer than a far one, in the coarse space.
        let mut near = a.clone();
        near[5] += 0.05;
        vector::normalize(&mut near);
        let mut far: Vec<f32> = (0..DIM).map(|i| ((i * 7 + 3) % 13) as f32 - 6.0).collect();
        vector::normalize(&mut far);
        let cos = |x: &[f32], y: &[f32]| x.iter().zip(y).map(|(a, b)| a * b).sum::<f32>();
        assert!(cos(&p.project(&a), &p.project(&near)) > cos(&p.project(&a), &p.project(&far)));
        assert_eq!(p.project_i8(&a).len(), COARSE_DIM);
        // Every bucket is used by some component (no dead buckets).
        let used: std::collections::BTreeSet<usize> = (0..DIM).map(|i| slot(0x7269_6c6c, i).0).collect();
        assert_eq!(used.len(), COARSE_DIM);
    }
}
