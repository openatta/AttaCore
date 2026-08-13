#!/bin/sh
# demo-plugin 的 SessionStart hook —— 整个文件内容会被当作 `sh -c` 的命令体
# 执行（见 crates/plugin/src/manifest.rs::install_hooks，command 字段就是文件
# 原文，不是"指向脚本的路径"）。这里只是证明 plugin.toml 的 hooks 配置面有内容、
# 格式正确；plugin.install 生命周期冒烟测试本身不会真的跑一轮对话去触发它
# （那需要把插件接进一个真实运行的 session，是更大的集成范围）。
echo '{"continue": true}'
