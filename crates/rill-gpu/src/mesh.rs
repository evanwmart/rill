//! Mesh loading for the model scene layer: Wavefront OBJ and STL, plus
//! directories of either (a model shipped as body/stand/tail parts loads as
//! one mesh, each file its own material id).
//!
//! OBJ is deliberately minimal: positions, normals, UVs, faces (any polygon
//! size, fan-triangulated), and `usemtl` tracked as a material index in
//! **MTL declaration order** — the contract model shaders key their
//! `material_id` switches to. STL carries no materials and no UVs at all,
//! which is why generic shaders auto-fit from the bounds this returns.
//!
//! Hand-rolled rather than a dependency: the subset is small, the failure
//! mode ("skip what you don't know, keep geometry") is ours to choose, and
//! the appliance keeps its dependency tree short.

use std::path::Path;

/// One vertex, flattened (non-indexed): the OBJ's three index spaces don't
/// share an index buffer without a dedup pass, and v1 favors simplicity —
/// dedup/indexing is a recorded optimization, not a correctness issue.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ModelVertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
    pub uv: [f32; 2],
    pub material_id: u32,
}

pub struct ModelMesh {
    pub vertices: Vec<ModelVertex>,
    /// Axis-aligned bounds, for logging and sanity checks.
    pub min: [f32; 3],
    pub max: [f32; 3],
    /// Material names in id order (MTL declaration order).
    pub materials: Vec<String>,
}

/// Parse the sibling MTL for `newmtl` declaration order — the id space the
/// shader's material switch uses.
fn mtl_order(path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("newmtl "))
        .map(|n| n.trim().to_string())
        .collect()
}

/// Load a mesh by what it is: an OBJ, an STL, or a directory of either
/// (concatenated, one material id per file, sorted by name so the ids are
/// stable across runs).
pub fn load(path: &Path) -> Result<ModelMesh, String> {
    if path.is_dir() {
        let mut parts: Vec<std::path::PathBuf> = std::fs::read_dir(path)
            .map_err(|e| format!("{}: {e}", path.display()))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.extension().is_some_and(|e| {
                    let e = e.to_ascii_lowercase();
                    e == "obj" || e == "stl"
                })
            })
            .collect();
        // Case-insensitive: "Tail" sorting before "body" is an ASCII
        // artefact, and part ids are something a shader author reads off a
        // directory listing.
        parts.sort_by_key(|p| p.to_string_lossy().to_lowercase());
        if parts.is_empty() {
            return Err(format!("{}: no meshes", path.display()));
        }
        let mut out: Option<ModelMesh> = None;
        for (i, part) in parts.iter().enumerate() {
            let mut mesh = load(part)?;
            let name = part
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            for v in &mut mesh.vertices {
                v.material_id = i as u32;
            }
            match &mut out {
                None => {
                    mesh.materials = vec![name];
                    out = Some(mesh);
                }
                Some(all) => {
                    all.vertices.append(&mut mesh.vertices);
                    all.materials.push(name);
                    for k in 0..3 {
                        all.min[k] = all.min[k].min(mesh.min[k]);
                        all.max[k] = all.max[k].max(mesh.max[k]);
                    }
                }
            }
        }
        return out.ok_or_else(|| format!("{}: no meshes", path.display()));
    }
    match path.extension().map(|e| e.to_ascii_lowercase()) {
        Some(e) if e == "stl" => load_stl(path),
        _ => load_obj(path),
    }
}

/// Load an STL (binary or ASCII). No materials, no UVs — every vertex gets
/// material 0 and a zero UV; the facet normal is shared by its three
/// corners, which is what gives STL its faceted look.
pub fn load_stl(path: &Path) -> Result<ModelMesh, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut vertices: Vec<ModelVertex> = Vec::new();
    let (mut min, mut max) = ([f32::MAX; 3], [f32::MIN; 3]);
    let mut push = |p: [f32; 3], n: [f32; 3], vertices: &mut Vec<ModelVertex>| {
        for i in 0..3 {
            min[i] = min[i].min(p[i]);
            max[i] = max[i].max(p[i]);
        }
        vertices.push(ModelVertex { position: p, normal: n, uv: [0.0, 0.0], material_id: 0 });
    };

    // Binary STL is 80 bytes of header, a u32 count, then 50 bytes per
    // triangle. The "solid" prefix is not proof of ASCII (plenty of binary
    // writers stamp it), so trust the arithmetic instead.
    let binary_len = |count: u32| 84usize + 50 * count as usize;
    let count = (bytes.len() >= 84)
        .then(|| u32::from_le_bytes(bytes[80..84].try_into().unwrap()))
        .filter(|c| binary_len(*c) == bytes.len());
    if let Some(count) = count {
        let f = |o: usize| f32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
        for t in 0..count as usize {
            let base = 84 + t * 50;
            let n = [f(base), f(base + 4), f(base + 8)];
            for c in 0..3 {
                let o = base + 12 + c * 12;
                push([f(o), f(o + 4), f(o + 8)], n, &mut vertices);
            }
        }
    } else {
        // ASCII: facet normal … / vertex x y z ×3.
        let text = String::from_utf8_lossy(&bytes);
        let mut normal = [0.0, 1.0, 0.0];
        for line in text.lines() {
            let mut parts = line.split_whitespace();
            match parts.next() {
                Some("facet") => {
                    let nums: Vec<f32> = parts.skip(1).filter_map(|s| s.parse().ok()).collect();
                    if nums.len() >= 3 {
                        normal = [nums[0], nums[1], nums[2]];
                    }
                }
                Some("vertex") => {
                    let nums: Vec<f32> = parts.filter_map(|s| s.parse().ok()).collect();
                    if nums.len() >= 3 {
                        push([nums[0], nums[1], nums[2]], normal, &mut vertices);
                    }
                }
                _ => {}
            }
        }
    }
    if vertices.is_empty() {
        return Err(format!("{}: no triangles", path.display()));
    }
    Ok(ModelMesh {
        vertices,
        min,
        max,
        materials: vec![path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "stl".into())],
    })
}

/// Load an OBJ (and its `mtllib`) into a flat triangle list.
pub fn load_obj(path: &Path) -> Result<ModelMesh, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;

    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut uvs: Vec<[f32; 2]> = Vec::new();
    let mut materials: Vec<String> = Vec::new();
    let mut current_mtl: u32 = 0;
    let mut vertices: Vec<ModelVertex> = Vec::new();
    let (mut min, mut max) = ([f32::MAX; 3], [f32::MIN; 3]);

    // A face corner "v/vt/vn" (each part optional after v), 1-based; negative
    // indices count back from the end, per spec.
    let resolve = |idx: i64, len: usize| -> Option<usize> {
        if idx > 0 {
            let i = (idx - 1) as usize;
            (i < len).then_some(i)
        } else if idx < 0 {
            len.checked_sub(idx.unsigned_abs() as usize)
        } else {
            None
        }
    };

    for line in text.lines() {
        let line = line.trim();
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("v") => {
                let mut p = [0.0f32; 3];
                for (i, s) in parts.take(3).enumerate() {
                    p[i] = s.parse().unwrap_or(0.0);
                }
                for i in 0..3 {
                    min[i] = min[i].min(p[i]);
                    max[i] = max[i].max(p[i]);
                }
                positions.push(p);
            }
            Some("vn") => {
                let mut n = [0.0f32; 3];
                for (i, s) in parts.take(3).enumerate() {
                    n[i] = s.parse().unwrap_or(0.0);
                }
                normals.push(n);
            }
            Some("vt") => {
                let mut t = [0.0f32; 2];
                for (i, s) in parts.take(2).enumerate() {
                    t[i] = s.parse().unwrap_or(0.0);
                }
                uvs.push(t);
            }
            Some("mtllib") => {
                if let Some(name) = parts.next()
                    && materials.is_empty()
                    && let Some(dir) = path.parent()
                {
                    materials = mtl_order(&dir.join(name));
                }
            }
            Some("usemtl") => {
                let name = parts.next().unwrap_or("").trim();
                current_mtl = materials
                    .iter()
                    .position(|m| m == name)
                    .map(|i| i as u32)
                    .unwrap_or_else(|| {
                        // An OBJ without an MTL still gets stable ids by
                        // first use.
                        materials.push(name.to_string());
                        (materials.len() - 1) as u32
                    });
            }
            Some("f") => {
                // Gather the polygon's corners, then fan-triangulate — the
                // wild has 20-gons, and a fan is exact for the convex faces
                // exporters emit.
                let mut corners: Vec<ModelVertex> = Vec::new();
                for corner in parts {
                    let mut it = corner.split('/');
                    let vi = it.next().and_then(|s| s.parse::<i64>().ok());
                    let ti = it.next().filter(|s| !s.is_empty()).and_then(|s| s.parse::<i64>().ok());
                    let ni = it.next().filter(|s| !s.is_empty()).and_then(|s| s.parse::<i64>().ok());
                    let Some(pos) = vi.and_then(|i| resolve(i, positions.len())) else { continue };
                    corners.push(ModelVertex {
                        position: positions[pos],
                        normal: ni
                            .and_then(|i| resolve(i, normals.len()))
                            .map(|i| normals[i])
                            .unwrap_or([0.0, 1.0, 0.0]),
                        uv: ti
                            .and_then(|i| resolve(i, uvs.len()))
                            .map(|i| uvs[i])
                            .unwrap_or([0.0, 0.0]),
                        material_id: current_mtl,
                    });
                }
                for i in 2..corners.len() {
                    vertices.push(corners[0]);
                    vertices.push(corners[i - 1]);
                    vertices.push(corners[i]);
                }
            }
            _ => {}
        }
    }

    if vertices.is_empty() {
        return Err(format!("{}: no triangles", path.display()));
    }
    Ok(ModelMesh { vertices, min, max, materials })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ngons_fan_and_materials_follow_mtl_order() {
        let dir = std::env::temp_dir().join(format!("obj-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(
            dir.join("t.mtl"),
            "newmtl body\nKd 1 0 0\nnewmtl chrome\nKd 0 1 0\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("t.obj"),
            "mtllib t.mtl\n\
             v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nv 2 0 0\n\
             vn 0 0 1\n\
             vt 0 0\n\
             usemtl chrome\n\
             f 1//1 2//1 3//1 4//1\n\
             usemtl body\n\
             f 1/1/1 2/1/1 5/1/1\n",
        )
        .unwrap();
        let mesh = load_obj(&dir.join("t.obj")).unwrap();
        // Quad fans to 2 triangles + 1 triangle = 9 vertices.
        assert_eq!(mesh.vertices.len(), 9);
        assert_eq!(mesh.materials, vec!["body".to_string(), "chrome".to_string()]);
        assert_eq!(mesh.vertices[0].material_id, 1, "chrome is MTL index 1");
        assert_eq!(mesh.vertices[6].material_id, 0, "body is MTL index 0");
        assert_eq!(mesh.max, [2.0, 1.0, 0.0]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
