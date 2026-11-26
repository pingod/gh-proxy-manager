//! 连通性/速度探测：直接用 curl 下载一个真实的 release 文件。
//!
//! 只测 raw README 会漏判“raw 可用、release 404/限速”的坏站，
//! 所以这里固定下载 [`crate::paths::DEFAULT_TEST_URL`]（或用户覆盖的地址）。

use std::process::{Command, Stdio};

/// 单次探测结果。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProbeResult {
    /// HTTP 状态码，0 表示连接失败。
    pub code: u16,
    /// 平均下载速度（字节/秒）。
    pub bps: u64,
    /// 连接失败时的错误说明。
    pub error: Option<String>,
}

impl ProbeResult {
    pub fn ok(&self) -> bool {
        matches!(self.code, 200 | 206)
    }

    /// 人类可读结论，例如 `✓ 可用 · 1234 KB/s` 或 `✗ 不可用（HTTP 404）`。
    pub fn verdict(&self) -> String {
        if self.ok() {
            format!("✓ 可用 · {} KB/s", self.bps / 1024)
        } else if let Some(err) = &self.error {
            format!("✗ 连接失败（{err}）")
        } else {
            format!("✗ 不可用（HTTP {}）", self.code)
        }
    }
}

/// 探测一个完整 URL。
pub fn probe(url: &str, max_secs: u64) -> ProbeResult {
    let out = Command::new("curl")
        .args([
            "-sSL",
            "--connect-timeout",
            "5",
            "--max-time",
            &max_secs.to_string(),
            "-o",
            "/dev/null",
            "-w",
            "%{http_code} %{speed_download}",
            url,
        ])
        .stdin(Stdio::null())
        .output();

    let out = match out {
        Ok(out) => out,
        Err(err) => {
            return ProbeResult {
                error: Some(format!("无法执行 curl: {err}")),
                ..Default::default()
            };
        }
    };

    let stdout = String::from_utf8_lossy(&out.stdout);
    let Some((code, bps)) = parse_curl_w(&stdout) else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return ProbeResult {
            error: Some(if err.is_empty() {
                "curl 没有返回结果".to_string()
            } else {
                err
            }),
            ..Default::default()
        };
    };

    ProbeResult {
        code,
        bps,
        error: None,
    }
}

/// 探测“前缀 + 测试文件”的组合。
pub fn probe_prefix(prefix: &str, test_url: &str) -> ProbeResult {
    probe(&format!("{prefix}{test_url}"), 40)
}

/// 解析 `curl -w '%{http_code} %{speed_download}'` 的输出。
pub fn parse_curl_w(text: &str) -> Option<(u16, u64)> {
    let last = text.lines().last()?.trim();
    let mut parts = last.split_whitespace();
    let code = parts.next()?.parse::<u16>().ok()?;
    let speed: f64 = parts.next()?.parse().ok()?;
    if !speed.is_finite() || speed < 0.0 {
        return None;
    }
    Some((code, speed as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_curl_output() {
        assert_eq!(parse_curl_w("200 1234567.000"), Some((200, 1_234_567)));
        assert_eq!(parse_curl_w("\n206 0.000\n"), Some((206, 0)));
        assert_eq!(parse_curl_w("garbage"), None);
        assert_eq!(parse_curl_w(""), None);
    }

    #[test]
    fn verdicts() {
        assert_eq!(
            ProbeResult {
                code: 200,
                bps: 2048,
                error: None
            }
            .verdict(),
            "✓ 可用 · 2 KB/s"
        );
        assert_eq!(
            ProbeResult {
                code: 404,
                bps: 0,
                error: None
            }
            .verdict(),
            "✗ 不可用（HTTP 404）"
        );
        assert_eq!(
            ProbeResult {
                code: 0,
                bps: 0,
                error: Some("timeout".into())
            }
            .verdict(),
            "✗ 连接失败（timeout）"
        );
    }

    #[test]
    fn ok_codes() {
        assert!(
            ProbeResult {
                code: 200,
                ..Default::default()
            }
            .ok()
        );
        assert!(
            ProbeResult {
                code: 206,
                ..Default::default()
            }
            .ok()
        );
        assert!(
            !ProbeResult {
                code: 403,
                ..Default::default()
            }
            .ok()
        );
    }
}
