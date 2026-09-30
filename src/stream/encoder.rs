//! Encoder thread: turns captured pixel regions into WebP tiles and sends them
//! to the browser, followed by a frame-end marker the browser acknowledges.
//!
//! Tile wire format (binary WebSocket message, little-endian):
//!   u8  type = 0x11
//!   u32 frame sequence
//!   u16 x, u16 y, u16 w, u16 h
//!   u8  format (0 = lossy WebP, 1 = lossless WebP)
//!   ... WebP bytes

use super::Out;
use std::sync::mpsc::Receiver;
use tokio::sync::mpsc::UnboundedSender;

pub const MSG_TILE: u8 = 0x11;

pub struct Tile {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// BGRX pixels, 4 bytes per pixel, rows packed.
    pub bgrx: Vec<u8>,
    pub lossless: bool,
}

pub struct Job {
    pub seq: u32,
    pub tiles: Vec<Tile>,
    pub tx: UnboundedSender<Out>,
    pub quality: f32,
}

pub fn bgrx_to_rgb(bgrx: &[u8], w: usize, h: usize) -> Vec<u8> {
    let n = w * h;
    let mut rgb = vec![0u8; n * 3];
    for (i, px) in bgrx.chunks_exact(4).take(n).enumerate() {
        rgb[i * 3] = px[2];
        rgb[i * 3 + 1] = px[1];
        rgb[i * 3 + 2] = px[0];
    }
    rgb
}

pub fn encode_webp(rgb: &[u8], w: u32, h: u32, lossless: bool, quality: f32) -> Option<Vec<u8>> {
    let enc = webp::Encoder::from_rgb(rgb, w, h);
    let mut cfg = webp::WebPConfig::new().ok()?;
    if lossless {
        cfg.lossless = 1;
        cfg.quality = 10.0; // effort: low = fast
        cfg.method = 0;
        cfg.exact = 1;
    } else {
        cfg.lossless = 0;
        cfg.quality = quality;
        cfg.method = 1;
        cfg.segments = 1;
        cfg.filter_strength = 0;
        cfg.sns_strength = 0;
    }
    cfg.thread_level = 1;
    enc.encode_advanced(&cfg).ok().map(|m| m.to_vec())
}

fn tile_message(seq: u32, t: &Tile, webp: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(14 + webp.len());
    m.push(MSG_TILE);
    m.extend_from_slice(&seq.to_le_bytes());
    for v in [t.x, t.y, t.w, t.h] {
        m.extend_from_slice(&(v as u16).to_le_bytes());
    }
    m.push(if t.lossless { 1 } else { 0 });
    m.extend_from_slice(webp);
    m
}

pub fn run(rx: Receiver<Job>) {
    while let Ok(job) = rx.recv() {
        let mut bytes = 0usize;
        for t in &job.tiles {
            let rgb = bgrx_to_rgb(&t.bgrx, t.w as usize, t.h as usize);
            match encode_webp(&rgb, t.w as u32, t.h as u32, t.lossless, job.quality) {
                Some(data) => {
                    bytes += data.len();
                    let _ = job.tx.send(Out::Bin(tile_message(job.seq, t, &data)));
                }
                None => tracing::warn!("webp encode failed for {}x{}", t.w, t.h),
            }
        }
        let _ = job
            .tx
            .send(Out::Text(format!(r#"{{"t":"f","s":{},"b":{}}}"#, job.seq, bytes)));
    }
}
