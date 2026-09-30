//! Rectangle helpers for damage tracking.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }
    pub fn area(&self) -> i64 {
        self.w.max(0) as i64 * self.h.max(0) as i64
    }
    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }
    pub fn union(&self, o: &Rect) -> Rect {
        let x0 = self.x.min(o.x);
        let y0 = self.y.min(o.y);
        let x1 = (self.x + self.w).max(o.x + o.w);
        let y1 = (self.y + self.h).max(o.y + o.h);
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }
    pub fn clip(&self, w: i32, h: i32) -> Rect {
        let x0 = self.x.clamp(0, w);
        let y0 = self.y.clamp(0, h);
        let x1 = (self.x + self.w).clamp(0, w);
        let y1 = (self.y + self.h).clamp(0, h);
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }
    pub fn intersects(&self, o: &Rect) -> bool {
        self.x < o.x + o.w && o.x < self.x + self.w && self.y < o.y + o.h && o.y < self.y + self.h
    }
    /// Split into pieces no larger than `max` × `max`.
    pub fn split(&self, max: i32) -> Vec<Rect> {
        let mut out = Vec::new();
        let mut y = self.y;
        while y < self.y + self.h {
            let h = max.min(self.y + self.h - y);
            let mut x = self.x;
            while x < self.x + self.w {
                let w = max.min(self.x + self.w - x);
                out.push(Rect::new(x, y, w, h));
                x += w;
            }
            y += h;
        }
        out
    }
}

/// Merge rectangles: overlapping or nearly-adjacent rectangles are combined
/// when the union wastes little area. Keeps the list short for encoding.
pub fn merge(mut rects: Vec<Rect>, screen_w: i32, screen_h: i32) -> Vec<Rect> {
    rects = rects
        .into_iter()
        .map(|r| r.clip(screen_w, screen_h))
        .filter(|r| !r.is_empty())
        .collect();
    if rects.len() > 64 {
        let mut u = rects[0];
        for r in &rects[1..] {
            u = u.union(r);
        }
        return vec![u];
    }
    const SLACK: i64 = 64 * 64;
    loop {
        let mut merged = false;
        'outer: for i in 0..rects.len() {
            for j in (i + 1)..rects.len() {
                let u = rects[i].union(&rects[j]);
                let waste = u.area() - rects[i].area() - rects[j].area();
                if rects[i].intersects(&rects[j]) || waste <= SLACK {
                    rects[i] = u;
                    rects.swap_remove(j);
                    merged = true;
                    break 'outer;
                }
            }
        }
        if !merged {
            break;
        }
    }
    rects
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn merging() {
        let r = merge(vec![Rect::new(0, 0, 10, 10), Rect::new(5, 5, 10, 10)], 100, 100);
        assert_eq!(r, vec![Rect::new(0, 0, 15, 15)]);
        let r = merge(vec![Rect::new(0, 0, 10, 10), Rect::new(900, 900, 10, 10)], 1000, 1000);
        assert_eq!(r.len(), 2);
        let r = merge(vec![Rect::new(-5, -5, 20, 20)], 10, 10);
        assert_eq!(r, vec![Rect::new(0, 0, 10, 10)]);
    }
    #[test]
    fn splitting() {
        let s = Rect::new(0, 0, 1000, 600).split(512);
        assert_eq!(s.len(), 4);
        assert_eq!(s.iter().map(|r| r.area()).sum::<i64>(), 600_000);
    }
}
