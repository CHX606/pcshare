use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::FrameError;

/// 单帧最大长度：16 MB（见总体方案 6.1 节）。
pub const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;
/// 匿名首帧（配对、直传握手等）允许的最大帧长度：64 KB。
pub const MAX_HANDSHAKE_FRAME_SIZE: usize = 64 * 1024;
/// 长度头最高位标记：为 1 时表示帧体经过 Deflate 压缩（仅在双方协商 `frame.deflate.v1` 后由发送方置位）。
pub const COMPRESSED_FRAME_FLAG: u32 = 0x8000_0000;
/// 触发控制帧压缩的最小 JSON 字节数：2 KB（小控制帧直发免压缩开销，大目录/搜索/软件列表帧高压缩比省带宽）。
pub const COMPRESS_THRESHOLD_BYTES: usize = 2048;
/// 分块读取帧体时的单次缓冲上限：64 KB，避免读完 4 字节长度头即按声明长度预分配大内存。
const FRAME_READ_CHUNK_SIZE: usize = 64 * 1024;

/// 写入一帧原始字节（4 字节大端长度 + 正文）。
pub async fn write_raw_frame<W>(writer: &mut W, payload: &[u8]) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    if payload.len() > MAX_FRAME_SIZE {
        return Err(FrameError::FrameTooLarge {
            size: payload.len(),
            max: MAX_FRAME_SIZE,
        });
    }
    let len_bytes = (payload.len() as u32).to_be_bytes();
    writer.write_all(&len_bytes).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

/// 将对象编码为 JSON 并单次写入一帧（4 字节大端长度头与正文合并写入，避免碎片包）。
pub async fn write_frame<W, T>(writer: &mut W, value: &T) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin + ?Sized,
    T: Serialize + ?Sized,
{
    let _ = write_frame_maybe_compressed(writer, value, false).await?;
    Ok(())
}

/// 按能力协商编码并写入一帧，返回实际写出的线上字节数（含 4 字节头）：
/// - 当 `compress == true` 且 JSON 正文 `>= COMPRESS_THRESHOLD_BYTES`（2 KB）时，使用快速 Deflate 压缩；
/// - 仅当压缩后体积明显小于原文时才置位 `COMPRESSED_FRAME_FLAG` 发送压缩帧，否则保持未压缩发送。
pub async fn write_frame_maybe_compressed<W, T>(
    writer: &mut W,
    value: &T,
    compress: bool,
) -> Result<usize, FrameError>
where
    W: AsyncWrite + Unpin + ?Sized,
    T: Serialize + ?Sized,
{
    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(&[0u8; 4]);
    serde_json::to_writer(&mut buf, value)?;
    let payload_len = buf.len() - 4;
    if payload_len > MAX_FRAME_SIZE {
        return Err(FrameError::FrameTooLarge {
            size: payload_len,
            max: MAX_FRAME_SIZE,
        });
    }

    if compress && payload_len >= COMPRESS_THRESHOLD_BYTES {
        use std::io::Write;
        let mut comp = Vec::with_capacity((payload_len / 2).max(256));
        comp.extend_from_slice(&[0u8; 4]);
        let mut enc = flate2::write::DeflateEncoder::new(&mut comp, flate2::Compression::fast());
        if enc.write_all(&buf[4..]).is_ok() && enc.finish().is_ok() {
            let comp_len = comp.len() - 4;
            if comp_len + 64 < payload_len && comp_len <= MAX_FRAME_SIZE {
                let hdr = (comp_len as u32) | COMPRESSED_FRAME_FLAG;
                comp[..4].copy_from_slice(&hdr.to_be_bytes());
                writer.write_all(&comp).await?;
                writer.flush().await?;
                return Ok(comp.len());
            }
        }
    }

    buf[..4].copy_from_slice(&(payload_len as u32).to_be_bytes());
    writer.write_all(&buf).await?;
    writer.flush().await?;
    Ok(buf.len())
}

/// 读取一帧（带单帧大小上限）。如果流在帧边界处干净结束（0 字节可读），返回 `Ok(None)`；
/// 如果读到一半断流，立即返回 `Err(FrameError::TruncatedFrame)`。
/// 帧体采用分块按需扩容读取，且解压输出严格限制在 `effective_max` 以内，防御超大长度头与压缩炸弹。
pub async fn read_optional_frame_limited<R, T>(
    reader: &mut R,
    max_size: usize,
) -> Result<Option<T>, FrameError>
where
    R: AsyncRead + Unpin + ?Sized,
    T: DeserializeOwned,
{
    let mut header = [0u8; 4];
    let mut header_read = 0usize;
    while header_read < 4 {
        let n = reader.read(&mut header[header_read..]).await?;
        if n == 0 {
            if header_read == 0 {
                return Ok(None);
            }
            return Err(FrameError::TruncatedFrame {
                expected: 4,
                got: header_read,
            });
        }
        header_read += n;
    }

    let effective_max = max_size.min(MAX_FRAME_SIZE);
    let raw_hdr = u32::from_be_bytes(header);
    let is_compressed = (raw_hdr & COMPRESSED_FRAME_FLAG) != 0;
    let len = (raw_hdr & !COMPRESSED_FRAME_FLAG) as usize;
    if len > effective_max {
        return Err(FrameError::FrameTooLarge {
            size: len,
            max: effective_max,
        });
    }

    let initial_cap = len.min(FRAME_READ_CHUNK_SIZE);
    let mut body = Vec::with_capacity(initial_cap);
    if len > 0 {
        let mut chunk = vec![0u8; initial_cap];
        while body.len() < len {
            let to_read = (len - body.len()).min(chunk.len());
            let n = reader.read(&mut chunk[..to_read]).await?;
            if n == 0 {
                return Err(FrameError::TruncatedFrame {
                    expected: len,
                    got: body.len(),
                });
            }
            body.extend_from_slice(&chunk[..n]);
        }
    }

    let decoded = if is_compressed {
        use std::io::Read;
        let decoder = flate2::read::DeflateDecoder::new(&body[..]);
        let mut limited = decoder.take((effective_max as u64).saturating_add(1));
        let mut plain = Vec::with_capacity(len.saturating_mul(2).min(FRAME_READ_CHUNK_SIZE));
        limited.read_to_end(&mut plain)?;
        if plain.len() > effective_max {
            return Err(FrameError::FrameTooLarge {
                size: plain.len(),
                max: effective_max,
            });
        }
        serde_json::from_slice(&plain)?
    } else {
        serde_json::from_slice(&body)?
    };
    Ok(Some(decoded))
}

/// 读取一帧。如果流在帧边界处干净结束（0 字节可读），返回 `Ok(None)`；
/// 如果读到一半断流，立即返回 `Err(FrameError::TruncatedFrame)`。
pub async fn read_optional_frame<R, T>(reader: &mut R) -> Result<Option<T>, FrameError>
where
    R: AsyncRead + Unpin + ?Sized,
    T: DeserializeOwned,
{
    read_optional_frame_limited(reader, MAX_FRAME_SIZE).await
}

/// 读取必有的一帧（带单帧大小上限）；如果对方一字节没发就关闭流，返回 `FrameError::UnexpectedEof`。
pub async fn read_frame_limited<R, T>(reader: &mut R, max_size: usize) -> Result<T, FrameError>
where
    R: AsyncRead + Unpin + ?Sized,
    T: DeserializeOwned,
{
    match read_optional_frame_limited(reader, max_size).await? {
        Some(frame) => Ok(frame),
        None => Err(FrameError::UnexpectedEof),
    }
}

/// 读取必有的一帧；如果对方一字节没发就关闭流，返回 `FrameError::UnexpectedEof`。
pub async fn read_frame<R, T>(reader: &mut R) -> Result<T, FrameError>
where
    R: AsyncRead + Unpin + ?Sized,
    T: DeserializeOwned,
{
    read_frame_limited(reader, MAX_FRAME_SIZE).await
}
