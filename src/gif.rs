//! 颜色量化（八叉树）+ 最近邻映射（15-bit 缓存）+ 帧差分工具。
//! GIF 容器、LZW 压缩、图形控制扩展统一交给成熟的 `gif` crate 编码。

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
// Floyd–Steinberg 抖动映射（RGB → 调色板索引帧）
// ============================================================

/// RGB 像素缓冲 → 索引帧。两行滚动误差缓冲，x 侧加 1 偏移免边界判断。
pub fn dither_map(rgb: &[u8], w: usize, h: usize, m: &mut ColorMapper) -> Vec<u8> {
    let mut out = vec![0u8; w * h];
    let mut cur = vec![[0i32; 3]; w + 2];
    let mut nxt = vec![[0i32; 3]; w + 2];
    for y in 0..h {
        let row = y * w;
        for x in 0..w {
            let p = (row + x) * 3;
            let i = x + 1;
            let old_r = rgb[p] as i32 + cur[i][0];
            let old_g = rgb[p + 1] as i32 + cur[i][1];
            let old_b = rgb[p + 2] as i32 + cur[i][2];
            let idx = m.map(old_r, old_g, old_b);
            out[row + x] = idx;
            let pc = m.color(idx);
            // 误差以裁剪后的原值为基准
            let er = old_r.clamp(0, 255) - pc[0] as i32;
            let eg = old_g.clamp(0, 255) - pc[1] as i32;
            let eb = old_b.clamp(0, 255) - pc[2] as i32;
            // 7/16 → 右邻；3/16 → 左下；5/16 → 正下；1/16 → 右下
            cur[i + 1][0] += (er * 7) >> 4;
            cur[i + 1][1] += (eg * 7) >> 4;
            cur[i + 1][2] += (eb * 7) >> 4;
            nxt[i - 1][0] += (er * 3) >> 4;
            nxt[i - 1][1] += (eg * 3) >> 4;
            nxt[i - 1][2] += (eb * 3) >> 4;
            nxt[i][0] += (er * 5) >> 4;
            nxt[i][1] += (eg * 5) >> 4;
            nxt[i][2] += (eb * 5) >> 4;
            nxt[i + 1][0] += er >> 4;
            nxt[i + 1][1] += eg >> 4;
            nxt[i + 1][2] += eb >> 4;
        }
        std::mem::swap(&mut cur, &mut nxt);
        nxt.iter_mut().for_each(|v| *v = [0; 3]);
    }
    out
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
// 单元测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

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
}
