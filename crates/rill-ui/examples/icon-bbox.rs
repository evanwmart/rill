fn main() {
    for name in ["rill-logo", "folder", "star"] {
        let icon = rill_ui::icons::icon(name).unwrap();
        let (pts, _) = icon.at(0.0, 0.0, 256.0);
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for p in &pts {
            x0 = x0.min(p.x); y0 = y0.min(p.y); x1 = x1.max(p.x); y1 = y1.max(p.y);
        }
        println!("{name}: ink {:.0}..{:.0} x {:.0}..{:.0} (of 256) center ({:.0},{:.0})",
            x0, x1, y0, y1, (x0+x1)/2.0, (y0+y1)/2.0);
    }
}
