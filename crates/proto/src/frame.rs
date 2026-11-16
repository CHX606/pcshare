use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::FrameError;

/// 单帧最大长度：16 MB（见总体方案 6.1 节）。
pub const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

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
    buf[..4].copy_from_slice(&(payload_len as u32).to_be_bytes());
    writer.write_all(&buf).await?;
    writer.flush().await?;
    Ok(())
}

/// 读取一帧。如果流在帧边界处干净结束（0 字节可读），返回 `Ok(None)`；
/// 如果读到一半断流，立即返回 `Err(FrameError::TruncatedFrame)`。
pub async fn read_optional_frame<R, T>(reader: &mut R) -> Result<Option<T>, FrameError>
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

    let len = u32::from_be_bytes(header) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(FrameError::FrameTooLarge {
            size: len,
            max: MAX_FRAME_SIZE,
        });
    }

    let mut body = vec![0u8; len];
    let mut body_read = 0usize;
    while body_read < len {
        let n = reader.read(&mut body[body_read..]).await?;
        if n == 0 {
            return Err(FrameError::TruncatedFrame {
                expected: len,
                got: body_read,
            });
        }
        body_read += n;
    }

    let decoded = serde_json::from_slice(&body)?;
    Ok(Some(decoded))
}

/// 读取必有的一帧；如果对方一字节没发就关闭流，返回 `FrameError::UnexpectedEof`。
pub async fn read_frame<R, T>(reader: &mut R) -> Result<T, FrameError>
where
    R: AsyncRead + Unpin + ?Sized,
    T: DeserializeOwned,
{
    match read_optional_frame(reader).await? {
        Some(frame) => Ok(frame),
        None => Err(FrameError::UnexpectedEof),
    }
}
