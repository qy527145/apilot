//! 捕获正文的落库压缩。
//!
//! 捕获表占的空间几乎全在请求 / 响应原文上，而它们是大段重复的 JSON（同一段
//! 对话历史每个请求都要带上），zstd 通常能压到十分之一。压缩只发生在**落库与
//! 读库这一层**：网关转发用的是内存里的原始字节，解压也只在打开详情 / 求值
//! 表达式时才做，不在请求路径上。
//!
//! 格式：`b"APZ1"` + 原始长度（u32 LE）+ zstd 压缩体。
//!
//! 为什么要有魔数：同一列里会长期并存两类行 —— 旧版本落库的未压缩 BLOB，和
//! 新版本压缩过的。读取时不能靠版本号或行号区分（老行不会被回填），只能让数据
//! 自己说明。JSON 正文永远以 `{` / `[` 开头，不会撞上 `APZ1`。
//!
//! 头里存原始长度有两个用处：读取时按它开缓冲并把「解压出来比记录的长」当成
//! 坏数据处理；`unpacked_len` 不真解压就能回答「这份报文解压后多大」，
//! 表达式筛选的读取预算靠它算，而不是靠压缩后的大小。

/// 魔数。改动格式时换一个，旧数据仍能被 `unpack` 原样读回。
const MAGIC: &[u8; 4] = b"APZ1";

/// 魔数 + u32 原始长度。
const HEADER_LEN: usize = 8;

/// 低于这个长度不值得压：zstd 的帧头加空块本身就要十几字节，
/// 而小报文（`{}`、错误体）在捕获里占比不低，逐条压只会白费 CPU。
const MIN_COMPRESS_LEN: usize = 1024;

/// 速度优先。捕获是旁路数据，压得慢会拖住落库任务；level 3 在 JSON 上
/// 已经能拿到接近 level 10 的压缩比。
const LEVEL: i32 = 3;

/// 压缩一段可选的 BLOB。
///
/// 三种情况原样返回，都必须**不丢数据**：太短、压完反而更大、压缩失败。
/// 压缩只是省空间，永远不该成为「这条捕获写不进去」的理由。
pub fn pack(bytes: Option<Vec<u8>>) -> Option<Vec<u8>> {
    let bytes = bytes?;
    // 头里的长度是 u32：超过 4GB 的报文（正常情况下不可能）就别硬套格式了。
    if bytes.len() < MIN_COMPRESS_LEN || bytes.len() > u32::MAX as usize {
        return Some(bytes);
    }

    let compressed = match zstd::bulk::compress(&bytes, LEVEL) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(len = bytes.len(), error = %e, "捕获报文压缩失败，按原样落库");
            return Some(bytes);
        }
    };

    // 算上头一起比较：压完只小几个字节，换来的只是读取时多一次解压。
    if HEADER_LEN + compressed.len() >= bytes.len() {
        return Some(bytes);
    }

    let mut out = Vec::with_capacity(HEADER_LEN + compressed.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&compressed);
    Some(out)
}

/// 解压一段可选的 BLOB。没有魔数的一律当成旧格式原样返回。
///
/// 有魔数却解不开（截断、写了半条、长度对不上）时返回 `None` 并留一条 warn：
/// 返回半份数据比返回空更糟 —— 界面上看起来是一份正常的报文，排查的人却会
/// 据此得出错误结论。数据本身已经坏了，这里能做的只是别让它继续骗人。
pub fn unpack(bytes: Option<Vec<u8>>) -> Option<Vec<u8>> {
    let bytes = bytes?;
    if !has_magic(&bytes) {
        return Some(bytes);
    }

    let Some(raw_len) = header_len(&bytes) else {
        tracing::warn!(len = bytes.len(), "捕获报文头部不完整，按损坏处理");
        return None;
    };

    // 按头部长度开缓冲：解压结果比记录的长会直接报错，顺带挡住了
    // 「头部被改成天文数字、解压时把内存吃光」这条坏路径。
    match zstd::bulk::decompress(&bytes[HEADER_LEN..], raw_len) {
        Ok(out) if out.len() == raw_len => Some(out),
        Ok(out) => {
            tracing::warn!(expected = raw_len, got = out.len(), "捕获报文解压后长度不符");
            None
        }
        Err(e) => {
            tracing::warn!(len = bytes.len(), error = %e, "捕获报文解压失败");
            None
        }
    }
}

/// 一份 BLOB 解压后的字节数，不真的解压。
///
/// 给读取预算用：压缩后的大小回答不了「解开要占多少内存」，而为了量尺寸
/// 先把几 MB 的报文解一遍，预算本身就成了内存问题。
pub fn unpacked_len(bytes: Option<&[u8]>) -> usize {
    match bytes {
        Some(b) => header_len(b).unwrap_or(b.len()),
        None => 0,
    }
}

fn has_magic(bytes: &[u8]) -> bool {
    bytes.len() >= MAGIC.len() && &bytes[..MAGIC.len()] == MAGIC
}

/// 魔数之后的原始长度。魔数不在或头部不完整时返回 `None`。
fn header_len(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < HEADER_LEN || !has_magic(bytes) {
        return None;
    }
    Some(u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一段像真实请求体的、可压缩的 JSON。
    fn json_body() -> Vec<u8> {
        let mut s = String::from(r#"{"model":"m","messages":["#);
        for i in 0..200 {
            s.push_str(&format!(
                r#"{{"role":"user","content":"第 {i} 条重复的对话内容，用来制造可压缩的重复"}},"#
            ));
        }
        s.push_str("]}");
        s.into_bytes()
    }

    #[test]
    fn roundtrip_returns_the_original_bytes() {
        let body = json_body();
        let packed = pack(Some(body.clone())).unwrap();
        assert_ne!(packed, body, "这段正文应当被压小");
        assert_eq!(unpack(Some(packed)).unwrap(), body);
    }

    #[test]
    fn compressible_bodies_actually_shrink() {
        let body = json_body();
        let packed = pack(Some(body.clone())).unwrap();
        assert!(
            packed.len() < body.len() / 2,
            "重复 JSON 的压缩比应明显，实际 {} -> {}",
            body.len(),
            packed.len()
        );
    }

    #[test]
    fn tiny_bodies_are_left_alone() {
        // 小报文压缩只会更大，而读它还要多一次解压；原样存。
        let small = br#"{"error":"bad request"}"#.to_vec();
        assert_eq!(pack(Some(small.clone())).unwrap(), small);
    }

    #[test]
    fn legacy_uncompressed_rows_are_returned_as_is() {
        // v15 之前的行没有魔数，读的时候必须原样给出，不能当成坏数据。
        let legacy = json_body();
        assert!(!has_magic(&legacy));
        assert_eq!(unpack(Some(legacy.clone())).unwrap(), legacy);
        assert_eq!(unpacked_len(Some(&legacy)), legacy.len());
    }

    #[test]
    fn corrupt_payloads_do_not_panic_and_yield_none() {
        // 魔数 + 长度 + 垃圾：解压失败。
        let mut bad = Vec::new();
        bad.extend_from_slice(MAGIC);
        bad.extend_from_slice(&1024u32.to_le_bytes());
        bad.extend_from_slice(b"not zstd at all");

        assert_eq!(unpack(Some(bad)), None, "坏数据不能当成功返回");
    }

    #[test]
    fn a_truncated_header_is_treated_as_corrupt() {
        // 写入中途崩溃可能留下半个头。魔数对但读不到长度，既不能当旧数据
        // 原样返回（会把 APZ1 当成正文显示），也不能 panic。
        assert_eq!(unpack(Some(b"APZ1".to_vec())), None);
        assert_eq!(unpacked_len(Some(b"APZ1")), 4, "量尺寸时退回原长度");
    }

    #[test]
    fn a_length_mismatch_is_rejected_instead_of_returning_partial_data() {
        let body = json_body();
        let mut packed = pack(Some(body.clone())).unwrap();
        // 把记录的长度改大：解压出来的字节数与头部对不上。
        let wrong = (body.len() as u32 + 1).to_le_bytes();
        packed[4..8].copy_from_slice(&wrong);

        assert_eq!(unpack(Some(packed)), None);
    }

    #[test]
    fn unpacked_len_reads_the_header_without_decompressing() {
        let body = json_body();
        let packed = pack(Some(body.clone())).unwrap();
        assert_eq!(unpacked_len(Some(&packed)), body.len());
    }

    #[test]
    fn none_stays_none() {
        assert!(pack(None).is_none());
        assert!(unpack(None).is_none());
        assert_eq!(unpacked_len(None), 0);
    }
}
