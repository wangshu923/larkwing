//! SSE 传输骨架的最小件:把字节流切成完整行(跨 chunk 缓冲)。
//! 解释 `data:` / 事件语义是各 provider 的事,这里只管 framing。
//! 按字节缓冲、整行再转字符串 —— 多字节字符(中文)跨 chunk 不会被切坏。

#[derive(Default)]
pub(crate) struct LineBuffer {
    buf: Vec<u8>,
}

impl LineBuffer {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut lines = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
            while matches!(line.last(), Some(b'\n' | b'\r')) {
                line.pop();
            }
            lines.push(String::from_utf8_lossy(&line).into_owned());
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_lines_across_chunks() {
        let mut lb = LineBuffer::default();
        assert!(lb.push(b"data: hel").is_empty());
        let lines = lb.push(b"lo\r\ndata: world\n");
        assert_eq!(lines, vec!["data: hello", "data: world"]);
    }

    #[test]
    fn multibyte_char_split_across_chunks_survives() {
        let mut lb = LineBuffer::default();
        let bytes = "data: 旺财\n".as_bytes();
        // 故意从多字节字符中间切开
        let lines1 = lb.push(&bytes[..8]);
        assert!(lines1.is_empty());
        let lines2 = lb.push(&bytes[8..]);
        assert_eq!(lines2, vec!["data: 旺财"]);
    }
}
