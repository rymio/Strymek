// Encoder benchmark: cargo run --release --example bench -- <screenshot.png>
#[path = "../src/stream/encoder.rs"]
#[allow(dead_code)]
mod encoder;
#[allow(dead_code)]
pub enum Out { Text(String), Bin(Vec<u8>), Close }
fn main() {
    let path = std::env::args().nth(1).expect("png path");
    let dec = png::Decoder::new(std::fs::File::open(path).unwrap());
    let mut r = dec.read_info().unwrap();
    let mut buf = vec![0; r.output_buffer_size()];
    let info = r.next_frame(&mut buf).unwrap();
    let (w, h) = (info.width, info.height);
    let ch = info.color_type.samples();
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    for px in buf[..info.buffer_size()].chunks(ch) { rgb.extend_from_slice(&px[..3]); }
    for (name, lossless, q) in [("lossy q80", false, 80.0), ("lossy q60", false, 60.0), ("lossless", true, 0.0)] {
        let t = std::time::Instant::now();
        let out = encoder::encode_webp(&rgb, w, h, lossless, q).unwrap();
        println!("{name:<10} {w}x{h}: {:>7} KB in {:>4} ms", out.len() / 1024, t.elapsed().as_millis());
    }
    // A typical keystroke-sized update: 64x32 lossless.
    let (tw, th) = (64u32, 32u32);
    let mut tile = Vec::new();
    for y in 400..400 + th { let s = ((y * w + 400) * 3) as usize; tile.extend_from_slice(&rgb[s..s + (tw * 3) as usize]); }
    let t = std::time::Instant::now();
    let out = encoder::encode_webp(&tile, tw, th, true, 0.0).unwrap();
    println!("keystroke tile 64x32 lossless: {} bytes in {} us", out.len(), t.elapsed().as_micros());
}
