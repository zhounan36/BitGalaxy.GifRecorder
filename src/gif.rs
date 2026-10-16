//! GIF89a 编码核心：八叉树量化 + LZW 压缩 + 帧差分工具
//! 零第三方依赖，流式写入，实时字节计数。

use std::io::{self, Write};

// ============================================================
// 八叉树量化 (Octree Color Quantization)
// ============================================================

const MAX_DEPTH: usize = 8;

struct OctreeNode {
    children: [u32; 8], // 子节点索引，0 = 空（哨兵）
    count: u64,
    r_sum: u64,
    g_sum: u64,
    b_sum: u64,
    is_leaf: bool,
    reducible_added: bool, // 是否已登记进可归并链表
}

fn new_node() -> OctreeNode {
    OctreeNode {
        children: [0; 8],
        count: 0,
        r_sum: 0,
        g_sum: 0,
        b_sum: 0,
        is_leaf: false,
        reducible_added: false,
    }
}

/// 基于八叉树的颜色量化器
pub struct OctreeQuantizer {
    nodes: Vec<OctreeNode>, // 索引 0 为哨兵，1 为根
    reducible: [Vec<u32>; MAX_DEPTH],
    leaf_count: usize,
    max_colors: usize,
}

impl OctreeQuantizer {
    pub fn new(max_colors: usize) -> Self {
        let mut nodes = Vec::with_capacity(2048);
        nodes.push(new_node()); // 0: 哨兵
        nodes.push(new_node()); // 1: 根
        Self {
            nodes,
            reducible: Default::default(),
            leaf_count: 0,
            max_colors: max_colors.max(2),
        }
    }

    fn alloc(&mut self) -> u32 {
        self.nodes.push(new_node());
        (self.nodes.len() - 1) as u32
    }

    /// 添加一个像素颜色样本
    pub fn add_pixel(&mut self, r: u8, g: u8, b: u8) {
        let mut idx = 1u32;
        for level in 0..MAX_DEPTH {
            let n = &mut self.nodes[idx as usize];
            n.count += 1;
            n.r_sum += r as u64;
            n.g_sum += g as u64;
            n.b_sum += b as u64;
            if level == MAX_DEPTH - 1 {
                if !n.is_leaf {
                    n.is_leaf = true;
                    self.leaf_count += 1;
                }
                if self.leaf_count > self.max_colors {
                    self.reduce_once();
                }
                return;
            }
            let bit = 7 - level as u32;
            let slot =
                (((r >> bit) & 1) << 2 | ((g >> bit) & 1) << 1 | ((b >> bit) & 1)) as usize;
            let child = self.nodes[idx as usize].children[slot];
            if child == 0 {
                let new_idx = self.alloc();
                self.nodes[idx as usize].children[slot] = new_idx;
                let parent = &mut self.nodes[idx as usize];
                if !parent.reducible_added {
                    parent.reducible_added = true;
                    self.reducible[level].push(idx);
                }
            }
            idx = self.nodes[idx as usize].children[slot];
        }
    }

    /// 归并一层：把某节点的子树收缩为单个叶子
    fn reduce_once(&mut self) {
        // 从最深层向浅层找仍带子节点的可归并节点
        for level in (0..MAX_DEPTH - 1).rev() {
            while let Some(node_idx) = self.reducible[level].pop() {
                if self.nodes[node_idx as usize].children.iter().all(|&c| c == 0) {
                    continue; // 已被归并过
                }
                let children = self.nodes[node_idx as usize].children;
                let absorbed = self.subtree_leaves(&children);
                // 统计信息在插入路径上已累加，断链 + 置叶即可
                {
                    let node = &mut self.nodes[node_idx as usize];
                    node.children = [0; 8];
                    node.is_leaf = true;
                    node.reducible_added = false;
                }
                self.leaf_count = self.leaf_count + 1 - absorbed;
                if self.leaf_count > self.max_colors {
                    self.reduce_once();
                }
                return;
            }
        }
    }

    fn subtree_leaves(&self, children: &[u32; 8]) -> usize {
        let mut stack: Vec<u32> = children.iter().copied().filter(|&c| c != 0).collect();
        let mut leaves = 0;
        while let Some(idx) = stack.pop() {
            let n = &self.nodes[idx as usize];
            if n.is_leaf && n.children.iter().all(|&c| c == 0) {
                leaves += 1;
            } else {
                for &c in &n.children {
                    if c != 0 {
                        stack.push(c);
                    }
                }
            }
        }
        leaves
    }

    /// 生成调色板（内容色，不含透明槽位）
    pub fn palette(&self) -> Vec<[u8; 3]> {
        let mut pal: Vec<([u64; 3], u64)> = Vec::new();
        // 遍历所有节点收集叶子（节点池按下标遍历即可）
        for n in &self.nodes[1..] {
            if n.is_leaf && n.count > 0 && n.children.iter().all(|&c| c == 0) {
                pal.push(([n.r_sum, n.g_sum, n.b_sum], n.count));
            }
        }
        pal.sort_by(|a, b| b.1.cmp(&a.1)); // 出现次数多的排前面
        pal.iter()
            .take(self.max_colors)
            .map(|(s, c)| [(s[0] / c) as u8, (s[1] / c) as u8, (s[2] / c) as u8])
            .collect()
    }
}

/// 从 RGB 像素缓冲构建调色板
/// `max_colors` 为内容色上限（透明槽位另算），建议传入 2^bits - 1
pub fn build_palette(rgb: &[u8], max_colors: usize) -> Vec<[u8; 3]> {
    let mut q = OctreeQuantizer::new(max_colors);
    // 采样上限：最多 40 万像素，超出则隔点跳采
    let px = rgb.len() / 3;
    let step = if px > 400_000 { px / 400_000 } else { 1 };
    let mut i = 0usize;
    while i * 3 + 2 < rgb.len() {
        q.add_pixel(rgb[i * 3], rgb[i * 3 + 1], rgb[i * 3 + 2]);
        i += step;
    }
    q.palette()
}

// ============================================================
// 颜色映射器：15-bit 缓存的最近邻查找
// ============================================================

pub struct ColorMapper {
    palette: Vec<[u8; 3]>,
    n_content: usize,
    cache: Vec<u16>, // 32768 项，u16::MAX = 未命中
}

const CACHE_MISS: u16 = u16::MAX;

impl ColorMapper {
    pub fn new(palette: Vec<[u8; 3]>) -> Self {
        let n_content = palette.len();
        Self {
            palette,
            n_content,
            cache: vec![CACHE_MISS; 32768],
        }
    }

    /// 取调色板颜色（Floyd–Steinberg 抖动误差回读用）
    pub fn color(&self, i: u8) -> [u8; 3] {
        self.palette[i as usize]
    }

    /// RGB 输入允许超界（抖动误差），内部会裁剪。返回内容色索引。
    pub fn map(&mut self, r: i32, g: i32, b: i32) -> u8 {
        let r = r.clamp(0, 255) as u8;
        let g = g.clamp(0, 255) as u8;
        let b = b.clamp(0, 255) as u8;
        let key = ((r as usize >> 3) << 10) | ((g as usize >> 3) << 5) | (b as usize >> 3);
        let cached = self.cache[key];
        if cached != CACHE_MISS {
            return cached as u8;
        }
        let (mut best, mut best_d) = (0u8, i64::MAX);
        for (i, c) in self.palette.iter().take(self.n_content).enumerate() {
            let dr = c[0] as i64 - r as i64;
            let dg = c[1] as i64 - g as i64;
            let db = c[2] as i64 - b as i64;
            // 加权平方距离（人眼对绿更敏感）
            let d = dr * dr * 2 + dg * dg * 4 + db * db * 3;
            if d < best_d {
                best_d = d;
                best = i as u8;
                if d == 0 {
                    break;
                }
            }
        }
        self.cache[key] = best as u16;
        best
    }
}

// ============================================================
// LZW 压缩（GIF 变体，LSB-first 位打包）
// ============================================================

/// 子块流写入器：GIF 数据按 ≤255 字节子块存储
struct SubBlockWriter {
    out: Vec<u8>,
    block: Vec<u8>,
}

impl SubBlockWriter {
    fn new() -> Self {
        Self { out: Vec::new(), block: Vec::new() }
    }
    fn push(&mut self, b: u8) {
        self.block.push(b);
        if self.block.len() == 255 {
            self.out.push(255);
            let mut b = std::mem::take(&mut self.block);
            self.out.append(&mut b);
        }
    }
    fn finish(mut self) -> Vec<u8> {
        if !self.block.is_empty() {
            self.out.push(self.block.len() as u8);
            let mut b = std::mem::take(&mut self.block);
            self.out.append(&mut b);
        }
        self.out.push(0); // 子块终结符
        self.out
    }
}

struct LzwBitWriter {
    out: SubBlockWriter,
    cur: u32,
    nbits: u32,
}

impl LzwBitWriter {
    fn new() -> Self {
        Self { out: SubBlockWriter::new(), cur: 0, nbits: 0 }
    }
    fn emit(&mut self, code: u16, size: u32) {
        self.cur |= (code as u32) << self.nbits;
        self.nbits += size;
        while self.nbits >= 8 {
            let b = self.cur as u8;
            self.out.push(b);
            self.cur >>= 8;
            self.nbits -= 8;
        }
    }
    fn flush(&mut self) {
        if self.nbits > 0 {
            let b = self.cur as u8;
            self.out.push(b);
            self.cur = 0;
            self.nbits = 0;
        }
    }
}

/// LZW 编码，返回完整子块流（含终结符 0x00）
/// 字典用 4096×256 平面数组，查表 O(1)
pub fn lzw_encode(min_code_size: u8, data: &[u8]) -> Vec<u8> {
    let clear = 1u16 << min_code_size;
    let eoi = clear + 1;
    let mut dict = vec![0xFFFFu16; 4096 * 256];
    let mut w = LzwBitWriter::new();
    let mut code_size = min_code_size as u32 + 1;
    let mut next_code = clear + 2;

    w.emit(clear, code_size);
    if data.is_empty() {
        w.emit(eoi, code_size);
        w.flush();
        return w.out.finish();
    }

    let mut prefix = data[0] as u16;
    for &k in &data[1..] {
        let slot = prefix as usize * 256 + k as usize;
        let next = dict[slot];
        if next != 0xFFFF {
            prefix = next;
        } else {
            w.emit(prefix, code_size);
            if next_code < 4096 {
                dict[slot] = next_code;
                next_code += 1;
                // 与解码端同步的码长增长规则
                if next_code as u32 == (1u32 << code_size) && code_size < 12 {
                    code_size += 1;
                }
            } else {
                // 字典满：发 CLEAR 重置
                w.emit(clear, code_size);
                dict.fill(0xFFFF);
                next_code = clear + 2;
                code_size = min_code_size as u32 + 1;
            }
            prefix = k as u16;
        }
    }
    w.emit(prefix, code_size);
    w.emit(eoi, code_size);
    w.flush();
    w.out.finish()
}

// ============================================================
// 帧差分工具
// ============================================================

/// 计算 cur 相对 prev 的变化包围盒 (x, y, w, h)。全等返回 None。
pub fn diff_bbox(cur: &[u8], prev: &[u8], w: usize, h: usize) -> Option<(usize, usize, usize, usize)> {
    debug_assert_eq!(cur.len(), w * h);
    debug_assert_eq!(prev.len(), w * h);
    let mut minx = usize::MAX;
    let mut miny = usize::MAX;
    let mut maxx = 0usize;
    let mut maxy = 0usize;
    for y in 0..h {
        let row = y * w;
        for x in 0..w {
            if cur[row + x] != prev[row + x] {
                if x < minx {
                    minx = x;
                }
                if y < miny {
                    miny = y;
                }
                if x > maxx {
                    maxx = x;
                }
                if y > maxy {
                    maxy = y;
                }
            }
        }
    }
    if minx == usize::MAX {
        return None;
    }
    Some((minx, miny, maxx - minx + 1, maxy - miny + 1))
}

/// 裁剪变化区域为 GIF 帧像素块：bbox 内与上一帧相同的像素写透明索引
pub fn crop_diff(cur: &[u8], prev: &[u8], w: usize, bbox: (usize, usize, usize, usize), trans: u8) -> Vec<u8> {
    let (bx, by, bw, bh) = bbox;
    let mut out = Vec::with_capacity(bw * bh);
    for y in by..by + bh {
        let row = y * w;
        for x in bx..bx + bw {
            let a = cur[row + x];
            let b = prev[row + x];
            out.push(if a == b { trans } else { a });
        }
    }
    out
}

// ============================================================
// GIF89a 容器写入器
// ============================================================

pub struct GifWriter<W: Write> {
    w: W,
    bits: u8,       // 调色板位数 4..=8
    trans: u8,      // 透明索引 = (1<<bits)-1
}

impl<W: Write> GifWriter<W> {
    /// palette 必须恰好 1<<bits 项（不足处补 0）
    pub fn new(mut w: W, width: u16, height: u16, palette: &[[u8; 3]], bits: u8) -> io::Result<Self> {
        debug_assert_eq!(palette.len(), 1usize << bits);
        let mut buf = Vec::with_capacity(64 + palette.len() * 3);
        buf.extend_from_slice(b"GIF89a");
        buf.extend_from_slice(&width.to_le_bytes());
        buf.extend_from_slice(&height.to_le_bytes());
        buf.push(0x80 | ((bits - 1) << 4) | (bits - 1)); // 全局色表标志 + 尺寸
        buf.push(0);
        buf.push(0);
        for c in palette {
            buf.extend_from_slice(c);
        }
        // Netscape 循环扩展：无限循环
        buf.extend_from_slice(&[0x21, 0xFF, 0x0B]);
        buf.extend_from_slice(b"NETSCAPE2.0");
        buf.extend_from_slice(&[0x03, 0x01, 0x00, 0x00, 0x00]);
        w.write_all(&buf)?;
        // 透明索引：1u32 先移位再收窄，避免 bits=8 时 u8 溢出
        Ok(Self { w, bits, trans: ((1u32 << bits) - 1) as u8 })
    }

    /// 写一帧。`idx` 为调色板索引流（fw*fh 个）。
    /// `use_trans`：帧内是否使用透明索引（帧差分帧为 true）。
    pub fn write_frame(
        &mut self,
        idx: &[u8],
        left: u16,
        top: u16,
        fw: u16,
        fh: u16,
        delay_cs: u16,
        use_trans: bool,
    ) -> io::Result<()> {
        let mut buf = Vec::with_capacity(32 + idx.len() / 2);
        // 图形控制扩展：处置方法=1(保留)，透明标志
        let packed = 0x04 | if use_trans { 0x01 } else { 0x00 };
        buf.extend_from_slice(&[0x21, 0xF9, 0x04, packed]);
        buf.extend_from_slice(&delay_cs.to_le_bytes());
        buf.push(self.trans);
        buf.push(0x00);
        // 图像描述符
        buf.push(0x2C);
        buf.extend_from_slice(&left.to_le_bytes());
        buf.extend_from_slice(&top.to_le_bytes());
        buf.extend_from_slice(&fw.to_le_bytes());
        buf.extend_from_slice(&fh.to_le_bytes());
        buf.push(0x00); // 无局部色表
        let mcs = self.bits.max(2);
        buf.push(mcs);
        let sub = lzw_encode(mcs, idx);
        buf.extend_from_slice(&sub);
        self.w.write_all(&buf)
    }

    pub fn finish(mut self) -> io::Result<()> {
        self.w.write_all(&[0x3B])?;
        self.w.flush()
    }
}

// ============================================================
// 单元测试（含迷你 LZW 解码器做往返验证）
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用迷你 LZW 解码器（GIF 变体）
    fn lzw_decode(mcs: u8, subblocks: &[u8]) -> Vec<u8> {
        // 拆子块
        let mut data = Vec::new();
        let mut i = 0usize;
        while i < subblocks.len() {
            let len = subblocks[i] as usize;
            if len == 0 {
                break;
            }
            data.extend_from_slice(&subblocks[i + 1..i + 1 + len]);
            i += 1 + len;
        }
        let clear = 1u16 << mcs;
        let eoi = clear + 1;
        // LSB-first 位读取器
        struct BitReader<'a> {
            data: &'a [u8],
            pos: usize,
            acc: u32,
            bits: u32,
        }
        impl<'a> BitReader<'a> {
            fn read(&mut self, size: u32) -> u16 {
                while self.bits < size {
                    self.acc |= (self.data[self.pos] as u32) << self.bits;
                    self.pos += 1;
                    self.bits += 8;
                }
                let code = (self.acc & ((1 << size) - 1)) as u16;
                self.acc >>= size;
                self.bits -= size;
                code
            }
        }
        let mut br = BitReader { data: &data, pos: 0, acc: 0, bits: 0 };
        // 字典：前缀 + 尾字节 → 字符串（用 (prefix_code, byte) 链）
        let mut dict_prefix = vec![0u16; 4096];
        let mut dict_byte = vec![0u8; 4096];
        let mut next_code = clear as usize + 2;
        let mut code_size = mcs as u32 + 1;
        let mut out: Vec<u8> = Vec::new();
        let reset = |dict_prefix: &mut Vec<u16>, dict_byte: &mut Vec<u8>, next_code: &mut usize, code_size: &mut u32| {
            for i in 0..clear as usize {
                dict_prefix[i] = u16::MAX;
                dict_byte[i] = i as u8;
            }
            *next_code = clear as usize + 2;
            *code_size = mcs as u32 + 1;
        };
        reset(&mut dict_prefix, &mut dict_byte, &mut next_code, &mut code_size);
        let mut prev: Option<u16> = None;
        // 沿字典链提取字符串（栈底为首字符）
        let lookup = |code: u16, dict_prefix: &Vec<u16>, dict_byte: &Vec<u8>| -> (Vec<u8>, u8) {
            let mut stack: Vec<u8> = Vec::new();
            let mut c = code;
            loop {
                stack.push(dict_byte[c as usize]);
                if dict_prefix[c as usize] == u16::MAX {
                    break;
                }
                c = dict_prefix[c as usize];
            }
            let first_char = stack[stack.len() - 1];
            stack.reverse();
            (stack, first_char)
        };
        loop {
            if br.pos > br.data.len() {
                break;
            }
            let code = br.read(code_size);
            if code == clear {
                reset(&mut dict_prefix, &mut dict_byte, &mut next_code, &mut code_size);
                prev = None;
                continue;
            }
            if code == eoi {
                break;
            }
            let (str_bytes, first_char) = if (code as usize) < next_code {
                // 基础表或动态表中已定义
                lookup(code, &dict_prefix, &dict_byte)
            } else if code as usize == next_code {
                // KwKwK 特殊情形：prev + prev 的首字符
                let p = prev.expect("KwKwK without prev");
                let (mut s, fc) = lookup(p, &dict_prefix, &dict_byte);
                s.push(fc);
                (s, fc)
            } else {
                panic!("corrupt LZW stream: code={} next={}", code, next_code);
            };
            out.extend_from_slice(&str_bytes);
            if let Some(p) = prev {
                if next_code < 4096 {
                    dict_prefix[next_code] = p;
                    dict_byte[next_code] = first_char;
                    next_code += 1;
                    // 注意：解码端字典比编码端滞后一拍，码长增长点要提前一拍
                    if next_code == (1usize << code_size) - 1 && code_size < 12 {
                        code_size += 1;
                    }
                }
            }
            prev = Some(code);
        }
        out
    }

    fn roundtrip(data: &[u8], mcs: u8) {
        let enc = lzw_encode(mcs, data);
        let dec = lzw_decode(mcs, &enc);
        assert_eq!(dec, data, "mcs={} len={}", mcs, data.len());
    }

    #[test]
    fn test_lzw_debug_codes() {
        // 基础往返冒烟
        let data: Vec<u8> = vec![65, 65, 66, 66, 65, 67, 67, 65];
        roundtrip(&data, 8);
    }

    #[test]
    fn test_lzw_roundtrip_basic() {
        for mcs in 2u8..=8 {
            // 数据字节必须 < 2^mcs，否则会被解码端当作 CLEAR/EOI 码
            let n = 1u32 << mcs;
            roundtrip(&[], mcs);
            roundtrip(&[0], mcs);
            roundtrip(&[1, 1, 1, 1], mcs);
            roundtrip(&(0..n as u16).map(|v| v as u8).collect::<Vec<u8>>(), mcs);
            let seq: Vec<u8> = (0..1000u32).map(|i| ((i * 37 + i / 11) % n) as u8).collect();
            roundtrip(&seq, mcs);
        }
    }

    #[test]
    fn test_lzw_roundtrip_long_repetitive() {
        // 长重复序列：触发字典增长与表满
        let mut data = Vec::new();
        let mut x = 12345u32;
        for _ in 0..200_000 {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            data.push((x >> 24) as u8);
            data.push((x >> 28) as u8);
        }
        roundtrip(&data, 8);
    }

    #[test]
    fn test_lzw_table_overflow() {
        // 高熵数据强制字典打满并多次重置
        let mut data = Vec::with_capacity(300_000);
        let mut x = 987654321u64;
        for _ in 0..300_000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            data.push((x >> 33) as u8);
        }
        roundtrip(&data, 8);
    }

    #[test]
    fn test_lzw_zero_length_runs() {
        let mut data = vec![5u8; 70000];
        data.extend(std::iter::repeat_n(9, 70000));
        data.extend((0..60000).map(|i| (i % 251) as u8));
        roundtrip(&data, 8);
    }

    #[test]
    fn test_octree_palette_size() {
        let mut rgb = Vec::new();
        for i in 0..256 {
            for j in 0..256 {
                rgb.push(i as u8);
                rgb.push(j as u8);
                rgb.push((i + j) as u8);
            }
        }
        for max in [15usize, 31, 63, 127, 255] {
            let pal = build_palette(&rgb, max);
            assert!(pal.len() <= max, "max={} got={}", max, pal.len());
            assert!(pal.len() >= 2);
        }
    }

    #[test]
    fn test_mapper_indices_valid() {
        let pal = build_palette(
            &[
                10, 20, 30, 200, 200, 200, 0, 0, 255, 255, 255, 0, 128, 128, 128, 0, 255, 0,
            ],
            5,
        );
        let mut m = ColorMapper::new(pal);
        for r in [0, 50, 128, 200, 255] {
            for g in [0, 50, 128, 200, 255] {
                for b in [0, 50, 128, 200, 255] {
                    let idx = m.map(r, g, b);
                    assert!(idx < m.n_content as u8);
                }
            }
        }
        // 超界输入（抖动）应被裁剪
        assert!(m.map(-30, 300, 128) < m.n_content as u8);
    }

    #[test]
    fn test_diff_bbox() {
        let w = 10usize;
        let h = 8usize;
        let a = vec![1u8; w * h];
        let b = a.clone();
        assert_eq!(diff_bbox(&b, &a, w, h), None);
        let mut c = a.clone();
        c[3 * w + 7] = 9;
        c[6 * w + 2] = 9;
        assert_eq!(diff_bbox(&c, &a, w, h), Some((2, 3, 6, 4)));
    }

    #[test]
    fn test_crop_diff() {
        let w = 4usize;
        let a = vec![1u8; 16];
        let mut c = a.clone();
        c[5] = 7; // (x=1,y=1)
        let bbox = diff_bbox(&c, &a, w, 4).unwrap();
        let out = crop_diff(&c, &a, w, bbox, 0xFF);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], 7);
        // 两像素变化，中间夹一个不变像素
        let mut d = a.clone();
        d[4] = 5;
        d[6] = 6;
        let bbox = diff_bbox(&d, &a, w, 4).unwrap();
        assert_eq!(bbox, (0, 1, 3, 1));
        let out = crop_diff(&d, &a, w, bbox, 0xFF);
        assert_eq!(out, vec![5, 0xFF, 6]);
    }

    #[test]
    fn test_gif_container_structure() {
        // 两帧 4x4 图，写完检查关键结构字节
        let pal: Vec<[u8; 3]> = (0..16).map(|i| [i * 16, i * 8, i * 4]).collect();
        let mut buf = Vec::new();
        {
            let mut g = GifWriter::new(&mut buf, 4, 4, &pal, 4).unwrap();
            let f1 = vec![3u8; 16];
            g.write_frame(&f1, 0, 0, 4, 4, 7, false).unwrap();
            let mut f2 = f1.clone();
            f2[0] = 5;
            let cropped = crop_diff(&f2, &f1, 4, diff_bbox(&f2, &f1, 4, 4).unwrap(), 15);
            g.write_frame(&cropped, 0, 0, 1, 1, 7, true).unwrap();
            g.finish().unwrap();
        }
        assert_eq!(&buf[0..6], b"GIF89a");
        assert_eq!(buf[6], 4); // width 低字节
        assert_eq!(buf[10], 0x80 | (3 << 4) | 3); // LSD packed: GCT 标志 + bits-1
        assert_eq!(*buf.last().unwrap(), 0x3B);
        // 结构化解析：跳过 header 后应恰好遇到 GCE+图像描述符 ×2，然后 trailer
        let mut p = 6 + 7 + 16 * 3 + 19; // header + LSD + GCT + NETSCAPE 扩展
        let mut frames = 0usize;
        while p < buf.len() && buf[p] != 0x3B {
            assert_eq!(buf[p], 0x21, "预期图形控制扩展");
            assert_eq!(buf[p + 1], 0xF9);
            // 第一帧不带透明标志，第二帧（帧差分）带
            assert_eq!(buf[p + 3] & 0x01, if frames == 1 { 1 } else { 0 });
            p += 8; // GCE: 21 F9 04 packed delay trans 00
            assert_eq!(buf[p], 0x2C, "预期图像描述符");
            p += 10; // 2C + left/top/w/h + packed
            p += 1; // LZW min code size
            while buf[p] != 0 {
                p += 1 + buf[p] as usize; // 跳子块
            }
            p += 1; // 子块终结符
            frames += 1;
        }
        assert_eq!(frames, 2);
        assert_eq!(p, buf.len() - 1); // 仅剩 trailer
    }
}
