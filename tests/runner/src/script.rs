//! TestScript 解析器 — 读取 `.test` 文件。
//!
//! 格式:
//! ```text
//! [第一个 >>>>>>>>>>>>>>>> 之前的内容 = 测试用例元信息]
//!
//! >>>>>>>>>>>>>>>>
//! [第 1 轮输入 — 用户消息]
//! <<<<<<<<<<<<<<<<
//! [第 1 轮预期输出描述 — 给 LLM 比对的自然语言]
//!
//! >>>>>>>>>>>>>>>>
//! [第 2 轮输入]
//! <<<<<<<<<<<<<<<<
//! [第 2 轮预期输出描述]
//! ```

const SEP_IN: &str = ">>>>>>>>>>>>>>>>";   // 16 >
const SEP_OUT: &str = "<<<<<<<<<<<<<<<<";  // 16 <

#[derive(Debug, Clone)]
pub struct TestCase {
    /// 第一个分隔符之前的元信息（用例名称、描述、前置条件）
    pub meta: String,
    /// 文件路径（用于报告）
    pub source_path: String,
    /// 多轮对话
    pub turns: Vec<Turn>,
}

#[derive(Debug, Clone)]
pub struct Turn {
    /// 轮次编号 (0-based)
    pub index: usize,
    /// 用户输入（发给 Agent 的消息）
    pub input: String,
    /// 预期输出描述（给 LLM 比对的自然语言）
    pub expected: String,
}

/// 解析 `.test` 文件。
pub fn parse_test_file(path: &std::path::Path) -> anyhow::Result<TestCase> {
    let content = std::fs::read_to_string(path)?;
    parse_test_script(&content, &path.display().to_string())
}

/// 解析 `.test` 脚本内容。
pub fn parse_test_script(content: &str, source_path: &str) -> anyhow::Result<TestCase> {
    let content = content.trim();
    if content.is_empty() {
        anyhow::bail!("empty test script");
    }

    // 找到第一个 >>>>>>>>>>>>>>>> 的位置
    let first_sep = content.find(SEP_IN);
    let (meta, body) = match first_sep {
        Some(pos) => {
            let m = content[..pos].trim().to_string();
            let b = &content[pos + SEP_IN.len()..];
            (m, b)
        }
        None => {
            // 没有分隔符 → 整个文件是单轮输入（无预期输出）
            return Ok(TestCase {
                meta: String::new(),
                source_path: source_path.to_string(),
                turns: vec![Turn {
                    index: 0,
                    input: content.to_string(),
                    expected: String::new(),
                }],
            });
        }
    };

    // 解析轮次
    let turns = parse_turns(body)?;
    if turns.is_empty() {
        anyhow::bail!("no turns found in test script (missing >>>>>>>>>>>>>>>> markers)");
    }

    Ok(TestCase { meta, source_path: source_path.to_string(), turns })
}

fn parse_turns(body: &str) -> anyhow::Result<Vec<Turn>> {
    let mut turns = Vec::new();
    let body = body.trim();

    // 按 >>>>>>>>>>>>>>>> 分割
    let blocks: Vec<&str> = body.split(SEP_IN).collect();
    for (i, block) in blocks.into_iter().enumerate() {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }

        // 按 <<<<<<<<<<<<<<<< 分割输入和预期输出
        let (input, expected) = match block.split_once(SEP_OUT) {
            Some((inp, exp)) => (inp.trim().to_string(), exp.trim().to_string()),
            None => (block.to_string(), String::new()),
        };

        turns.push(Turn { index: i, input, expected });
    }

    Ok(turns)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_turn() {
        let script = "\
# Test case
This is a test.

>>>>>>>>>>>>>>>>
Hello, create a file.
<<<<<<<<<<<<<<<<
Should create a file and report success.
";
        let tc = parse_test_script(script, "test.test").unwrap();
        assert_eq!(tc.meta, "# Test case\nThis is a test.");
        assert_eq!(tc.turns.len(), 1);
        assert_eq!(tc.turns[0].input, "Hello, create a file.");
        assert_eq!(tc.turns[0].expected, "Should create a file and report success.");
    }

    #[test]
    fn parse_multi_turn() {
        let script = "\
Multi-turn test

>>>>>>>>>>>>>>>>
Turn 1 input.
<<<<<<<<<<<<<<<<
Turn 1 expected.

>>>>>>>>>>>>>>>>
Turn 2 input.
<<<<<<<<<<<<<<<<
Turn 2 expected.
";
        let tc = parse_test_script(script, "test.test").unwrap();
        assert_eq!(tc.turns.len(), 2);
        assert_eq!(tc.turns[0].input, "Turn 1 input.");
        assert_eq!(tc.turns[1].input, "Turn 2 input.");
    }

    #[test]
    fn parse_no_expected() {
        let script = "\
>>>>>>>>>>>>>>>>
Just input, no expected output marker.
";
        let tc = parse_test_script(script, "test.test").unwrap();
        assert_eq!(tc.turns.len(), 1);
        assert_eq!(tc.turns[0].input, "Just input, no expected output marker.");
        assert!(tc.turns[0].expected.is_empty());
    }

    #[test]
    fn parse_empty_meta() {
        let script = "\
>>>>>>>>>>>>>>>>
Input only.
<<<<<<<<<<<<<<<<
Expected.
";
        let tc = parse_test_script(script, "test.test").unwrap();
        assert!(tc.meta.is_empty());
        assert_eq!(tc.turns.len(), 1);
    }
}
