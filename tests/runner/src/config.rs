//! 测试系统的唯一配置源：仓库根 `.env`（`export KEY=VALUE` 格式）。
//!
//! - `ANTHROPIC_MODEL`：Agent 主流程用的正式模型，录制和回放必须用同一个值——
//!   model 名字是请求的一部分，回放时对不上会被报成 `params` 分歧。
//! - `ANTHROPIC_SMALL_FAST_MODEL`：只用于 LLM 比对裁判（`comparator.rs`），跟主
//!   Agent 流程的录制/回放无关，不受此约束，可以随便用便宜档模型。

use std::path::Path;

#[derive(Debug, Clone)]
pub struct TestModelConfig {
    pub base_url: String,
    pub auth_token: String,
    pub model: String,
    pub fast_model: String,
}

/// 解析 `.env`，并把每个键写入当前进程环境（`std::env::set_var`）。
///
/// 这样无论 api 模式还是 cli 模式启动，`api_runner.rs`/`comparator.rs` 里已有的
/// `std::env::var("ANTHROPIC_MODEL")` 之类的读取都能拿到值，不需要外部 shell
/// 先手动 `source` 一遍配置文件。
pub fn load_env_config(path: &Path) -> anyhow::Result<TestModelConfig> {
    let vars = parse_env_file(path)?;
    for (k, v) in &vars {
        // SAFETY: test-runner 是单线程启动阶段调用这里，之后才 spawn 并发任务；
        // 不存在与其他线程同时读写 env 的竞争。
        unsafe {
            std::env::set_var(k, v);
        }
    }

    let get = |key: &str| -> Option<String> {
        vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    };

    let base_url = get("ANTHROPIC_BASE_URL")
        .ok_or_else(|| anyhow::anyhow!("ANTHROPIC_BASE_URL not found in {}", path.display()))?;
    let auth_token = get("ANTHROPIC_AUTH_TOKEN")
        .ok_or_else(|| anyhow::anyhow!("ANTHROPIC_AUTH_TOKEN not found in {}", path.display()))?;
    let model = get("ANTHROPIC_MODEL").unwrap_or_else(|| "claude-sonnet-4-6".into());
    let fast_model = get("ANTHROPIC_SMALL_FAST_MODEL").unwrap_or_else(|| "claude-haiku-4-5".into());

    Ok(TestModelConfig {
        base_url,
        auth_token,
        model,
        fast_model,
    })
}

/// 解析录制轮次标识：`--round` CLI 参数 > `ATTA_RECORD_ROUND` 环境变量 >
/// 今天的日期（`YYYY-MM-DD`，UTC）。
///
/// 轮次是录制目录的一层（`tests/fixtures/cassettes/{scenario}/{mode}/{round}/`），
/// 不同轮次物理上是不同目录，互不覆盖：
/// 换模型/改 prompt 后开新一轮，旧轮次的录制数据原样留在磁盘上，不会被静默覆盖或
/// 和新数据混在同一个文件里追加。默认按日期分轮，同一天内的多次录制自然归到同一轮
/// （不用每次手动想一个轮次名），过了这天再录就是新的一轮。
pub fn resolve_record_round(explicit: Option<String>) -> String {
    if let Some(r) = explicit {
        return r;
    }
    if let Ok(r) = std::env::var("ATTA_RECORD_ROUND") {
        if !r.is_empty() {
            return r;
        }
    }
    let now = time::OffsetDateTime::now_utc();
    let rfc3339 = now
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "0000-00-00T00:00:00Z".into());
    rfc3339[..10].to_string()
}

/// 解析 `export KEY=VALUE` 格式的配置文件为键值对列表（不做 env 注入）。
/// 用于 cli 模式给 daemon 子进程按需传递 env。
pub fn parse_env_file(path: &Path) -> anyhow::Result<Vec<(String, String)>> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
    let mut vars = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("export ") {
            if let Some((k, v)) = rest.split_once('=') {
                let v = v.trim().trim_matches('"').trim_matches('\'');
                vars.push((k.trim().to_string(), v.to_string()));
            }
        }
    }
    Ok(vars)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一个测试函数里把三级优先级全部断言完，避免和其他测试并发读写同一个
    /// 进程级环境变量 `ATTA_RECORD_ROUND` 产生竞争（cargo test 默认多线程并发跑）。
    #[test]
    fn round_resolution_priority() {
        // 1) 显式参数优先于一切，哪怕环境变量也设了。
        unsafe { std::env::set_var("ATTA_RECORD_ROUND", "should-be-ignored") };
        assert_eq!(resolve_record_round(Some("explicit".into())), "explicit");

        // 2) 没有显式参数时用环境变量。
        assert_eq!(resolve_record_round(None), "should-be-ignored");

        // 3) 都没有时落到今天的 UTC 日期，格式 YYYY-MM-DD。
        unsafe { std::env::remove_var("ATTA_RECORD_ROUND") };
        let round = resolve_record_round(None);
        assert_eq!(round.len(), 10, "expected YYYY-MM-DD, got: {round}");
        assert_eq!(round.as_bytes()[4], b'-');
        assert_eq!(round.as_bytes()[7], b'-');
        assert!(round.chars().all(|c| c.is_ascii_digit() || c == '-'));
    }
}
