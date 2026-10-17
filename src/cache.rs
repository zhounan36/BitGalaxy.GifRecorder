//! 录制帧缓存：录屏先落盘为无损帧流，供编辑器预览/剪切/裁切后导出 GIF。
//!
//! 文件格式（小端，单文件自包含）：
//! ```text
//! Header : "BGRC" | u32 version=1 | u32 width | u32 height
//! Record*: u8 kind(0=关键帧/1=差分帧) | u16 delay_cs | u32 payload_len | payload
//!   关键帧 payload = RLE(width*height*3 的全量 RGB)
//!   差分帧 payload = u16 x | u16 y | u16 w | u16 h | RLE(w*h*3 的局部 RGB)
//! ```
//! RLE 控制字节：`b<0x80` = 后跟 (b+1) 个 literal 像素（3 字节/像素）；
//! `b>=0x80` = 重复像素 (b&0x7F)+2 次（后跟 3 字节像素色）。
//! 屏幕内容大面积同色，压缩率高且编解码零依赖、速度接近 memcpy。
//! 无变化帧不写记录，delay 挂到下一写入帧（与 GIF 帧差分语义一致）。

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 4] = b"BGRC";
const VERSION: u32 = 1;

// ============================================================
// RLE
// ============================================================

fn rle_encode(src: &[u8], out: &mut Vec<u8>) {
    debug_assert_eq!(src.len() % 3, 0);
    let n = src.len() / 3;
    let mut i = 0usize;
    let mut lit = 0usize; // 当前未 flush 的 literal 段长度（像素），区间 [i-lit, i)
    while i < n {
        let mut run = 1usize;
        while i + run < n && run < 128 && src[(i + run) * 3..(i + run) * 3 + 3] == src[i * 3..i * 3 + 3]
        {
            run += 1;
        }
        if run >= 3 {
            // flush literal [i-lit, i)
            let mut s = i - lit;
            while s < i {
                let c = (i - s).min(127);
                out.push((c - 1) as u8);
                out.extend_from_slice(&src[s * 3..(s + c) * 3]);
                s += c;
            }
            lit = 0;
            out.push(0x80 | (run - 2) as u8);
            out.extend_from_slice(&src[i * 3..i * 3 + 3]);
        } else {
            lit += run;
        }
        i += run;
    }
    let mut s = n - lit;
    while s < n {
        let c = (n - s).min(127);
        out.push((c - 1) as u8);
        out.extend_from_slice(&src[s * 3..(s + c) * 3]);
        s += c;
    }
}

fn rle_decode(inp: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(inp.len() * 3);
    let mut p = 0usize;
    while p < inp.len() {
        let b = inp[p];
        p += 1;
        if b & 0x80 == 0 {
            let c = (b as usize) + 1;
            out.extend_from_slice(&inp[p..p + c * 3]);
            p += c * 3;
        } else {
            let c = ((b & 0x7f) as usize) + 2;
            let px = [inp[p], inp[p + 1], inp[p + 2]];
            p += 3;
            out.extend(std::iter::repeat(px).take(c).flatten());
        }
    }
}

/// RGB 帧的变化包围盒 (x, y, w, h)。全等返回 None。
/// （gif::diff_bbox 是索引帧语义，这里需要 RGB 版）
fn rgb_diff_bbox(cur: &[u8], prev: &[u8], w: usize, h: usize) -> Option<(usize, usize, usize, usize)> {
    let mut minx = usize::MAX;
    let mut miny = usize::MAX;
    let mut maxx = 0usize;
    let mut maxy = 0usize;
    for y in 0..h {
        let row = y * w * 3;
        for x in 0..w {
            let p = row + x * 3;
            if cur[p] != prev[p] || cur[p + 1] != prev[p + 1] || cur[p + 2] != prev[p + 2] {
                minx = minx.min(x);
                miny = miny.min(y);
                maxx = maxx.max(x);
                maxy = maxy.max(y);
            }
        }
    }
    if minx == usize::MAX {
        return None;
    }
    Some((minx, miny, maxx - minx + 1, maxy - miny + 1))
}

// ============================================================
// 写入端（录制线程）
// ============================================================

pub struct FrameCacheWriter {
    file: BufWriter<File>,
    w: usize,
    h: usize,
    key_interval: u32, // 每隔多少条记录强制关键帧
    rec_count: u32,
    since_key: u32,
    written: u64,
    prev: Vec<u8>,
    scratch: Vec<u8>,
}

impl FrameCacheWriter {
    pub fn create(path: &Path, w: usize, h: usize, key_interval: u32) -> std::io::Result<Self> {
        let mut file = BufWriter::new(File::create(path)?);
        file.write_all(MAGIC)?;
        file.write_all(&VERSION.to_le_bytes())?;
        file.write_all(&(w as u32).to_le_bytes())?;
        file.write_all(&(h as u32).to_le_bytes())?;
        file.flush()?;
        Ok(Self {
            written: 4 + 4 * 3,
            file,
            w,
            h,
            key_interval: key_interval.clamp(4, 120),
            rec_count: 0,
            since_key: 0,
            prev: vec![0u8; w * h * 3],
            scratch: Vec::new(),
        })
    }

    pub fn written_bytes(&self) -> u64 {
        self.written
    }

    /// 尝试写入一帧（RGB，w*h*3）。与上一帧相同则不写（返回 false），
    /// 调用方应把 delay 累计到下一帧。首帧必为关键帧。
    pub fn write_frame(&mut self, rgb: &[u8], delay_cs: u32) -> std::io::Result<bool> {
        debug_assert_eq!(rgb.len(), self.w * self.h * 3);
        let (w, h) = (self.w, self.h);
        let force_key = self.rec_count == 0 || self.since_key >= self.key_interval;
        let (bx, by, bw, bh) = if force_key {
            (0usize, 0, w, h)
        } else {
            match rgb_diff_bbox(rgb, &self.prev, w, h) {
                Some(b) => b,
                None => return Ok(false), // 无变化
            }
        };

        self.scratch.clear();
        if force_key {
            rle_encode(rgb, &mut self.scratch);
        } else {
            // 逐行取差分块（跳过块外区域）
            let mut rows = Vec::with_capacity(bh * bw * 3);
            for y in 0..bh {
                let s = (by + y) * w * 3 + bx * 3;
                rows.extend_from_slice(&rgb[s..s + bw * 3]);
            }
            rle_encode(&rows, &mut self.scratch);
        }

        let kind: u8 = if force_key { 0 } else { 1 };
        let mut head = [0u8; 7];
        head[0] = kind;
        head[1..3].copy_from_slice(&(delay_cs.min(600) as u16).to_le_bytes());
        head[3..7].copy_from_slice(&(self.scratch.len() as u32).to_le_bytes());
        self.file.write_all(&head)?;
        if !force_key {
            let mut d = [0u8; 8];
            d[0..2].copy_from_slice(&(bx as u16).to_le_bytes());
            d[2..4].copy_from_slice(&(by as u16).to_le_bytes());
            d[4..6].copy_from_slice(&(bw as u16).to_le_bytes());
            d[6..8].copy_from_slice(&(bh as u16).to_le_bytes());
            self.file.write_all(&d)?;
        }
        self.file.write_all(&self.scratch)?;

        // 维护 prev
        if force_key {
            self.prev.copy_from_slice(rgb);
        } else {
            for y in 0..bh {
                let d = (by + y) * w * 3 + bx * 3;
                self.prev[d..d + bw * 3].copy_from_slice(&rgb[d..d + bw * 3]);
            }
        }

        self.written += head.len() as u64 + self.scratch.len() as u64 + if force_key { 0 } else { 8 };
        self.rec_count += 1;
        self.since_key = if force_key { 0 } else { self.since_key + 1 };
        Ok(true)
    }

    pub fn finish(mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

// ============================================================
// 读取端（编辑器 / 导出）
// ============================================================

struct Entry {
    offset: u64,   // 记录头（kind 字节）在文件中的偏移
    kind: u8,
    delay_cs: u32,
    plen: u32,
}

pub struct FrameCache {
    path: PathBuf,
    pub w: usize,
    pub h: usize,
    entries: Vec<Entry>,
}

fn read_exact_at(f: &mut BufReader<File>, off: u64, buf: &mut [u8]) -> std::io::Result<()> {
    f.seek(SeekFrom::Start(off))?;
    f.read_exact(buf)
}

impl FrameCache {
    /// 打开并顺序扫描记录头，建立索引（不解压，速度接近顺序读）
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let mut f = BufReader::new(File::open(path)?);
        let mut head = [0u8; 16];
        f.read_exact(&mut head)?;
        if &head[0..4] != MAGIC {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "帧缓存格式错误",
            ));
        }
        // 头部：MAGIC(4) + version(4) + width(4) + height(4)
        let w = u32::from_le_bytes(head[8..12].try_into().unwrap()) as usize;
        let h = u32::from_le_bytes(head[12..16].try_into().unwrap()) as usize;
        let mut entries = Vec::new();
        let mut off = 16u64;
        loop {
            let mut rh = [0u8; 7];
            match read_exact_at(&mut f, off, &mut rh) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            }
            let kind = rh[0];
            let delay_cs = u16::from_le_bytes(rh[1..3].try_into().unwrap()) as u32;
            let plen = u32::from_le_bytes(rh[3..7].try_into().unwrap());
            if kind > 1 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "帧缓存记录损坏",
                ));
            }
            entries.push(Entry { offset: off, kind, delay_cs, plen });
            let extra = if kind == 1 { 8u64 } else { 0 };
            off += 7 + extra + plen as u64;
        }
        Ok(Self { path: path.to_path_buf(), w, h, entries })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn delay_cs(&self, i: usize) -> u32 {
        self.entries[i].delay_cs
    }

    /// 索引 i 的时间轴位置（毫秒，含）
    pub fn time_ms_before(&self, i: usize) -> u64 {
        self.entries[..i].iter().map(|e| e.delay_cs as u64).sum::<u64>() * 10
    }

    pub fn total_ms(&self) -> u64 {
        self.time_ms_before(self.entries.len())
    }

    /// 解码一条记录到 buf（buf 为 w*h*3 的 RGB）
    fn apply_entry(
        f: &mut BufReader<File>,
        e: &Entry,
        buf: &mut [u8],
        w: usize,
        tmp: &mut Vec<u8>,
    ) -> std::io::Result<()> {
        if e.kind == 0 {
            let mut comp = vec![0u8; e.plen as usize];
            read_exact_at(f, e.offset + 7, &mut comp)?;
            let mut full = Vec::new();
            rle_decode(&comp, &mut full);
            debug_assert_eq!(full.len(), buf.len());
            buf.copy_from_slice(&full);
        } else {
            let mut rh = [0u8; 15];
            read_exact_at(f, e.offset, &mut rh)?;
            let bx = u16::from_le_bytes(rh[7..9].try_into().unwrap()) as usize;
            let by = u16::from_le_bytes(rh[9..11].try_into().unwrap()) as usize;
            let bw = u16::from_le_bytes(rh[11..13].try_into().unwrap()) as usize;
            let bh = u16::from_le_bytes(rh[13..15].try_into().unwrap()) as usize;
            let mut comp = vec![0u8; e.plen as usize];
            read_exact_at(f, e.offset + 15, &mut comp)?;
            rle_decode(&comp, tmp);
            for y in 0..bh {
                let s = y * bw * 3;
                let d = (by + y) * w * 3 + bx * 3;
                buf[d..d + bw * 3].copy_from_slice(&tmp[s..s + bw * 3]);
            }
        }
        Ok(())
    }

    /// 解码第 i 帧到 buf（从最近关键帧顺序解，最多回溯 key_interval 帧）
    pub fn decode_into(&self, i: usize, buf: &mut Vec<u8>) -> std::io::Result<()> {
        assert!(i < self.entries.len());
        let (w, h) = (self.w, self.h);
        buf.clear();
        buf.resize(w * h * 3, 0);
        let start = self.entries[..=i].iter().rposition(|e| e.kind == 0).unwrap_or(0);
        let mut f = BufReader::new(File::open(&self.path)?);
        let mut tmp = Vec::new();
        for e in &self.entries[start..=i] {
            Self::apply_entry(&mut f, e, buf, w, &mut tmp)?;
        }
        Ok(())
    }

    /// 顺序流式解码 [a, b)（b 为开区间终点），逐帧回调 (rgb, delay_cs)
    pub fn decode_range<F: FnMut(&[u8], u32)>(
        &self,
        a: usize,
        b: usize,
        mut cb: F,
    ) -> std::io::Result<()> {
        let (w, h) = (self.w, self.h);
        let b = b.min(self.entries.len());
        let start = self.entries[..a.min(self.entries.len())]
            .iter()
            .rposition(|e| e.kind == 0)
            .unwrap_or(0);
        let mut buf = vec![0u8; w * h * 3];
        let mut f = BufReader::new(File::open(&self.path)?);
        let mut tmp = Vec::new();
        for (idx, e) in self.entries[start..b].iter().enumerate() {
            let i = start + idx;
            Self::apply_entry(&mut f, e, &mut buf, w, &mut tmp)?;
            if i >= a {
                cb(&buf, e.delay_cs);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn gradient(w: usize, h: usize, seed: u8) -> Vec<u8> {
        (0..w * h)
            .flat_map(|i| {
                let v = ((i * 7 + seed as usize) % 256) as u8;
                [v, 255 - v, (v / 2) + seed]
            })
            .collect()
    }

    #[test]
    fn test_rle_roundtrip() {
        let mut src = vec![0u8; 300 * 3]; // 大片同色
        for (i, b) in gradient(30, 5, 3).into_iter().enumerate() {
            if i < 150 {
                src[i] = b;
            }
        }
        let mut enc = Vec::new();
        rle_encode(&src, &mut enc);
        assert!(enc.len() < src.len(), "RLE 应有压缩");
        let mut dec = Vec::new();
        rle_decode(&enc, &mut dec);
        assert_eq!(dec, src);
    }

    #[test]
    fn test_cache_write_scan_decode() {
        let path = std::env::temp_dir().join("bgr_cache_test.bin");
        let (w, h) = (64, 48);
        let f0 = gradient(w, h, 0);
        let mut f1 = f0.clone();
        // 制造局部差异
        for y in 10..20 {
            for x in 5..25 {
                let p = (y * w + x) * 3;
                f1[p] = 200;
                f1[p + 1] = 10;
                f1[p + 2] = 10;
            }
        }
        let mut wr = FrameCacheWriter::create(&path, w, h, 30).unwrap();
        assert!(wr.write_frame(&f0, 4).unwrap());
        assert!(wr.write_frame(&f1, 4).unwrap());
        assert!(!wr.write_frame(&f1, 4).unwrap(), "相同帧不应写入");
        wr.finish().unwrap();

        let cache = FrameCache::open(&path).unwrap();
        assert_eq!((cache.w, cache.h), (w, h));
        assert_eq!(cache.len(), 2);
        let mut buf = Vec::new();
        cache.decode_into(0, &mut buf).unwrap();
        assert_eq!(buf, f0);
        cache.decode_into(1, &mut buf).unwrap();
        assert_eq!(buf, f1);

        // 顺序流式
        let mut seen = 0;
        cache.decode_range(0, 2, |rgb, d| {
            assert_eq!(d, 4);
            assert_eq!(rgb.len(), w * h * 3);
            seen += 1;
        })
        .unwrap();
        assert_eq!(seen, 2);
        let _ = fs::remove_file(&path);
    }
}
