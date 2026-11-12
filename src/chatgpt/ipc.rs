use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::time::{Duration, Instant};

const MAX_FRAME: usize = 256 * 1024 * 1024;

#[derive(Default)]
pub(super) struct Decoder {
    bytes: Vec<u8>,
    partial_since: Option<Instant>,
}
impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Value>> {
        self.bytes.extend_from_slice(bytes);
        let mut messages = Vec::new();
        let mut offset = 0;
        while self.bytes.len() - offset >= 4 {
            let size =
                u32::from_le_bytes(self.bytes[offset..offset + 4].try_into().unwrap()) as usize;
            ensure!(size > 0 && size <= MAX_FRAME, "IPC 帧长度无效：{size}");
            if self.bytes.len() - offset < size + 4 {
                break;
            }
            messages.push(
                serde_json::from_slice(&self.bytes[offset + 4..offset + 4 + size])
                    .context("IPC 帧不是有效 JSON")?,
            );
            offset += 4 + size;
        }
        self.bytes.drain(..offset);
        if self.bytes.is_empty() {
            // 完整快照可能很大；解析后仅保留一个读取块的容量供后续增量复用。
            self.bytes.shrink_to(64 * 1024);
            self.partial_since = None;
        } else if offset > 0 || self.partial_since.is_none() {
            self.partial_since = Some(Instant::now());
        }
        self.check_timeout()?;
        Ok(messages)
    }
    pub fn check_timeout(&self) -> Result<()> {
        ensure!(
            self.partial_since
                .is_none_or(|t| t.elapsed() <= Duration::from_secs(3)),
            "IPC 部分帧接收超时"
        );
        Ok(())
    }
}

/// Immer 的 path 是字符串/整数数组，不是 JSON Pointer。
pub(super) fn apply_patches(state: &mut Value, patches: &Value) -> Result<()> {
    for patch in patches.as_array().context("patches 必须是数组")? {
        let path = patch["path"].as_array().context("patch path 必须是数组")?;
        let op = patch["op"].as_str().context("patch 缺少 op")?;
        ensure!(
            ["add", "replace", "remove"].contains(&op),
            "未知 patch 操作：{op}"
        );
        let value = if op == "remove" {
            None
        } else {
            Some(patch.get("value").context("patch 缺少 value")?.clone())
        };
        if path.is_empty() {
            *state = value.context("不支持删除状态根节点")?;
            continue;
        }
        let mut node = &mut *state;
        for key in &path[..path.len() - 1] {
            node = match key {
                Value::String(key) => node.as_object_mut().and_then(|o| o.get_mut(key)),
                Value::Number(key) => key
                    .as_u64()
                    .and_then(|i| node.as_array_mut().and_then(|a| a.get_mut(i as usize))),
                _ => None,
            }
            .context("patch 路径不存在或类型错误")?;
        }
        let key = &path[path.len() - 1];
        match (node, key) {
            (Value::Object(object), Value::String(key)) => {
                if op != "add" {
                    ensure!(object.contains_key(key), "patch 目标字段不存在");
                }
                if let Some(value) = value {
                    object.insert(key.clone(), value);
                } else {
                    object.remove(key);
                }
            }
            (Value::Array(array), Value::Number(key)) => {
                let index = key.as_u64().context("patch 数组索引无效")? as usize;
                if op == "add" {
                    ensure!(index <= array.len(), "patch 数组插入越界");
                    array.insert(index, value.unwrap());
                } else {
                    ensure!(index < array.len(), "patch 数组索引越界");
                    if let Some(value) = value {
                        array[index] = value;
                    } else {
                        array.remove(index);
                    }
                }
            }
            _ => bail!("patch 路径类型不匹配"),
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub(super) struct Connection {
    stream: std::os::unix::net::UnixStream,
    decoder: Decoder,
}
#[cfg(target_os = "macos")]
impl Connection {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let stream = std::os::unix::net::UnixStream::connect(path).context("无法连接桌面 IPC")?;
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;
        Ok(Self {
            stream,
            decoder: Decoder::default(),
        })
    }
    pub fn send(&mut self, message: &Value) -> Result<()> {
        use std::io::Write;
        let body = serde_json::to_vec(message)?;
        ensure!(body.len() <= MAX_FRAME, "IPC 请求过大");
        self.stream.write_all(&(body.len() as u32).to_le_bytes())?;
        self.stream.write_all(&body)?;
        Ok(())
    }
    pub fn receive(&mut self, timeout: Duration) -> Result<Vec<Value>> {
        use std::io::{ErrorKind, Read};
        self.stream
            .set_read_timeout(Some(timeout.max(Duration::from_millis(1))))?;
        let mut bytes = [0; 65536];
        match self.stream.read(&mut bytes) {
            Ok(0) => bail!("桌面 IPC 连接已关闭"),
            Ok(n) => self.decoder.push(&bytes[..n]),
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                self.decoder.check_timeout()?;
                Ok(Vec::new())
            }
            Err(error) => Err(error).context("读取桌面 IPC 失败"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn fragmented_and_multiple_frames() {
        let body = br#"{"type":"broadcast"}"#;
        let mut frame = (body.len() as u32).to_le_bytes().to_vec();
        frame.extend(body);
        let mut decoder = Decoder::default();
        assert!(decoder.push(&frame[..2]).unwrap().is_empty());
        assert!(decoder.push(&frame[2..7]).unwrap().is_empty());
        let mut tail = frame[7..].to_vec();
        tail.extend(&frame);
        assert_eq!(decoder.push(&tail).unwrap().len(), 2);
    }
    #[test]
    fn large_snapshot_does_not_retain_large_receive_buffer() {
        let body = serde_json::to_vec(&json!({"text": "x".repeat(256 * 1024)})).unwrap();
        let mut frame = (body.len() as u32).to_le_bytes().to_vec();
        frame.extend(body);
        let mut decoder = Decoder::default();
        let messages = decoder.push(&frame).unwrap();
        assert_eq!(messages[0]["text"].as_str().unwrap().len(), 256 * 1024);
        assert!(decoder.bytes.is_empty());
        assert!(decoder.bytes.capacity() <= 64 * 1024);
    }

    #[test]
    fn malformed_and_stalled_frames_fail() {
        assert!(Decoder::default().push(&u32::MAX.to_le_bytes()).is_err());
        let mut decoder = Decoder::default();
        decoder.push(&[1]).unwrap();
        decoder.partial_since = Some(Instant::now() - Duration::from_secs(4));
        assert!(decoder.check_timeout().is_err());
    }
    #[test]
    fn patches_preserve_array_semantics_and_fail_on_missing_paths() {
        let mut value = json!({"requests":[],"turns":[{"status":"inProgress"}]});
        apply_patches(
            &mut value,
            &json!([
                {"op":"add","path":["requests",0],"value":{"id":1}},
                {"op":"replace","path":["turns",0,"status"],"value":"completed"},
                {"op":"remove","path":["requests",0]}
            ]),
        )
        .unwrap();
        assert_eq!(
            value,
            json!({"requests":[],"turns":[{"status":"completed"}]})
        );
        assert!(
            apply_patches(
                &mut value,
                &json!([{"op":"replace","path":["missing"],"value":1}])
            )
            .is_err()
        );
    }
}
