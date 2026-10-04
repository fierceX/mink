# 快速开始

> 更新日期：2026-10-04

目标：在项目目录让 Mink 读取文件并给出简洁说明。前置：Rust 1.94+、可访问的 DeepSeek/OpenAI-compatible 模型端点及 API key。

## 从源码安装

```bash
git clone https://github.com/fierceX/mink.git
cd mink
cargo build --release -p mink-cli
export PATH="$PWD/target/release:$PATH"
```

构建后 `mink --help` 应显示终端参数。`mink-cli` 是 workspace 内部包，不使用 `cargo install mink-cli`。也可从 [GitHub Releases](https://github.com/fierceX/mink/releases)选择匹配平台的二进制。

## 配置模型

在自己的 shell 中设置 `DEEPSEEK_API_KEY`，然后选择模型别名 `flash`。密钥不要写入仓库。兼容端点可以使用以下 TOML 字符串；`--config` 接受字符串，不读取文件路径：

```bash
mink --base-url https://your-provider.example/v1 --model your-model \
  --config '[context]
max_context = "64K"
context_reserve_tokens = 12000' -i
```

模型真实窗口必须按 provider 声明填写。完整优先级见[配置参考](../reference/configuration.md)。

## 提交首个任务

```bash
cd /path/to/your/project
mink -m flash --session first-look --enabled-tools Read,Glob,Grep \
  "先读取 README 和目录结构，用中文说明这个项目如何运行，列出你实际查看的文件。"
```

成功表现：工具调用返回实际文件内容，最终回复说明项目入口并列出已读文件。若 README 不存在，工具会返回错误，模型应据实际目录继续；没有 API key 或模型请求失败会明确结束。此示例只启用只读工具。

## 继续同一个会话

```bash
mink -m flash --session first-look --tui
```

空闲时输入下一条任务，运行中 Enter 可提交引导。只有正式上下文交接才会回显 `Added to context`。退出后可用 `--continue` 恢复最近会话。

## 下一步

[终端操作](../guides/terminal.md)、[Web 工作台](../guides/web.md)、[Rust 集成](../integration/rust.md)或[Python 集成](../integration/python.md)。请求失败时看[故障排查](../guides/troubleshooting.md)。
